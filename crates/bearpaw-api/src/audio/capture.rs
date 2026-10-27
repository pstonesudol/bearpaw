//! CoreAudio (via `cpal`) input, converted to the one format the rest of the
//! feature understands: mono, 16 kHz, f32 in `[-1, 1]`.
//!
//! The device is owned by a single dedicated thread because `cpal::Stream` is
//! not `Send` on every platform. Everything else — the poll loop, HTTP
//! handlers — talks to that thread through channels and a status struct, so a
//! device that takes seconds to open (or never opens) can never stall scanner
//! control.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use serde::Serialize;

use super::{SegmentEvent, MAX_QUEUED_CHUNKS, RECORDING_SAMPLE_RATE};

/// How long to wait before retrying a device that could not be opened.
const RETRY_DELAY: Duration = Duration::from_secs(3);
/// How often the owner thread wakes to notice a control command or a stream
/// error. Also bounds how long `stop()` can take.
const IDLE_TICK: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub is_default: bool,
    pub channels: u16,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureStatusSnapshot {
    pub running: bool,
    pub connected: bool,
    pub device: Option<String>,
    pub last_error: Option<String>,
    /// Peak level of the most recent chunk, in dBFS (`-90` is silence).
    pub level_db: f32,
    /// Chunks dropped because the writer fell more than [`MAX_QUEUED_CHUNKS`]
    /// behind. Zero in normal operation; non-zero means the recording has gaps.
    pub dropped_chunks: u64,
}

/// Shared, lock-light status the API can read without touching the device.
#[derive(Debug, Default)]
pub struct CaptureStatus {
    running: AtomicBool,
    connected: AtomicBool,
    device: Mutex<Option<String>>,
    error: Mutex<Option<String>>,
    peak_bits: AtomicU32,
    dropped: AtomicU64,
}

impl CaptureStatus {
    fn set_device(&self, name: Option<String>) {
        if let Ok(mut slot) = self.device.lock() {
            *slot = name;
        }
    }

    fn set_error(&self, error: Option<String>) {
        if let Ok(mut slot) = self.error.lock() {
            *slot = error;
        }
    }

    fn set_level(&self, peak: f32) {
        self.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
    }

    fn record_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> CaptureStatusSnapshot {
        let peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed));
        let level_db = if peak <= 0.0 {
            -90.0
        } else {
            (20.0 * peak.log10()).clamp(-90.0, 0.0)
        };
        CaptureStatusSnapshot {
            running: self.running.load(Ordering::Relaxed),
            connected: self.connected.load(Ordering::Relaxed),
            device: self.device.lock().ok().and_then(|d| d.clone()),
            last_error: self.error.lock().ok().and_then(|e| e.clone()),
            level_db,
            dropped_chunks: self.dropped.load(Ordering::Relaxed),
        }
    }
}

pub fn capture_status(status: &CaptureStatus) -> CaptureStatusSnapshot {
    status.snapshot()
}

/// Hands audio and hit events to the segmenter on one channel, so their order
/// is the order they were produced in.
///
/// The queue is unbounded but self-limiting: the audio callback drops frames
/// once `MAX_QUEUED_CHUNKS` are waiting. Control events are never dropped —
/// losing a hit boundary would be worse than losing 20 ms of audio.
#[derive(Clone)]
pub struct EventBus {
    tx: Sender<SegmentEvent>,
    pending: Arc<AtomicUsize>,
    status: Arc<CaptureStatus>,
}

impl EventBus {
    pub fn new(tx: Sender<SegmentEvent>, status: Arc<CaptureStatus>) -> Self {
        Self {
            tx,
            pending: Arc::new(AtomicUsize::new(0)),
            status,
        }
    }

    pub fn send_samples(&self, samples: Vec<f32>) {
        if samples.is_empty() {
            return;
        }
        if self.pending.load(Ordering::Relaxed) >= MAX_QUEUED_CHUNKS {
            self.status.record_drop();
            return;
        }
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(SegmentEvent::Samples(samples)).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn send(&self, event: SegmentEvent) {
        let _ = self.tx.send(event);
    }

    /// For the receiving side: mark one queued chunk consumed.
    pub fn completed_one(&self) {
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug)]
pub enum CaptureCommand {
    Stop,
    /// Reopen the configured device — used when the user changes the selection.
    Reopen,
}

/// Owns the capture thread.
pub struct CaptureHandle {
    ctrl: Sender<CaptureCommand>,
    status: Arc<CaptureStatus>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    pub fn status(&self) -> Arc<CaptureStatus> {
        self.status.clone()
    }

    pub fn stop(&mut self) {
        let _ = self.ctrl.send(CaptureCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// Exact CoreAudio device name. `None` means the system default input.
    pub device: Option<String>,
}

/// Open the configured input and feed `bus` until stopped.
///
/// Returns immediately with a handle even when the device is missing: the
/// thread retries every [`RETRY_DELAY`] so a USB adapter replugged after launch
/// starts working without a restart. That mirrors the scanner's own
/// re-resolve loop.
pub fn spawn(config: CaptureConfig, bus: EventBus, status: Arc<CaptureStatus>) -> CaptureHandle {
    let (ctrl_tx, ctrl_rx) = std::sync::mpsc::channel();
    status.running.store(true, Ordering::Relaxed);
    let thread_status = status.clone();
    let thread = thread::Builder::new()
        .name("bearpaw-audio-capture".to_string())
        .spawn(move || run_capture_loop(config, bus, thread_status, ctrl_rx))
        .ok();
    CaptureHandle {
        ctrl: ctrl_tx,
        status,
        thread,
    }
}

fn run_capture_loop(
    config: CaptureConfig,
    bus: EventBus,
    status: Arc<CaptureStatus>,
    ctrl: Receiver<CaptureCommand>,
) {
    let mut was_connected = false;
    loop {
        match open_stream(&config, &bus, &status) {
            Ok(stream) => {
                was_connected = true;
                status.connected.store(true, Ordering::Relaxed);
                status.set_error(None);
                if let Err(e) = stream.play() {
                    status.set_error(Some(format!("audio_stream_play_failed: {e}")));
                }
                // Wake regularly: the stream's error callback cannot tell this
                // thread directly that the device vanished.
                let mut command = None;
                loop {
                    match ctrl.recv_timeout(IDLE_TICK) {
                        Ok(CaptureCommand::Stop) => {
                            command = Some(CaptureCommand::Stop);
                            break;
                        }
                        Ok(CaptureCommand::Reopen) => {
                            command = Some(CaptureCommand::Reopen);
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if status.snapshot().last_error.is_some() {
                                break;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            command = Some(CaptureCommand::Stop);
                            break;
                        }
                    }
                }
                drop(stream);
                status.connected.store(false, Ordering::Relaxed);
                if was_connected {
                    bus.send(SegmentEvent::DeviceLost);
                }
                if matches!(command, Some(CaptureCommand::Stop)) {
                    break;
                }
            }
            Err(error) => {
                if was_connected {
                    bus.send(SegmentEvent::DeviceLost);
                    was_connected = false;
                }
                status.connected.store(false, Ordering::Relaxed);
                status.set_error(Some(error));
                status.set_level(0.0);
                match ctrl.recv_timeout(RETRY_DELAY) {
                    Ok(CaptureCommand::Stop)
                    | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                    Ok(CaptureCommand::Reopen)
                    | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
    status.running.store(false, Ordering::Relaxed);
    status.connected.store(false, Ordering::Relaxed);
    status.set_level(0.0);
    bus.send(SegmentEvent::Stop);
}

fn open_stream(
    config: &CaptureConfig,
    bus: &EventBus,
    status: &Arc<CaptureStatus>,
) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = match &config.device {
        Some(wanted) if !wanted.trim().is_empty() => host
            .input_devices()
            .map_err(|e| format!("audio_enumerate_failed: {e}"))?
            .find(|d| d.name().ok().as_deref() == Some(wanted.trim()))
            .ok_or_else(|| format!("audio_device_not_found: {wanted}"))?,
        _ => host
            .default_input_device()
            .ok_or_else(|| "audio_device_not_found: no default input".to_string())?,
    };

    let name = device
        .name()
        .unwrap_or_else(|_| "Unknown input".to_string());
    let supported = device
        .default_input_config()
        .map_err(|e| format!("audio_config_unavailable: {e}"))?;
    let stream_config: StreamConfig = supported.config();
    status.set_device(Some(name));

    let bus_for_stream = bus.clone();
    let status_for_stream = status.clone();
    let sink = move |samples: Vec<f32>| {
        let peak = samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
        status_for_stream.set_level(peak);
        bus_for_stream.send_samples(samples);
    };
    let status_for_error = status.clone();
    let on_error = move |message: String| {
        status_for_error.set_error(Some(format!("audio_stream_error: {message}")));
    };

    match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, sink, on_error),
        SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, sink, on_error),
        SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, sink, on_error),
        SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, sink, on_error),
        other => return Err(format!("audio_sample_format_unsupported: {other:?}")),
    }
    .map_err(|e| format!("audio_stream_build_failed: {e}"))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut sink: impl FnMut(Vec<f32>) + Send + 'static,
    mut on_error: impl FnMut(String) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + 'static,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels).max(1);
    let mut resampler = LinearResampler::new(
        f64::from(config.sample_rate.0),
        f64::from(RECORDING_SAMPLE_RATE),
    );
    device.build_input_stream(
        config,
        move |data: &[T], _info| {
            let mut mono: Vec<f32> = Vec::with_capacity(data.len() / channels);
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|s| f32::from_sample(*s)).sum();
                mono.push(sum / channels as f32);
            }
            let mut converted = Vec::with_capacity(mono.len());
            resampler.process(&mono, &mut converted);
            sink(converted);
        },
        move |err| on_error(err.to_string()),
        None,
    )
}

/// Linear-interpolation resampler. Radio speech through a 16 kHz output does
/// not justify a windowed-sinc filter, and this keeps the audio callback cheap.
struct LinearResampler {
    /// Input samples advanced per output sample (`from / to`).
    step: f64,
    /// Absolute input position of the next output sample.
    next: f64,
    /// Absolute input position of the sample in `prev`.
    index: f64,
    prev: f32,
    started: bool,
}

impl LinearResampler {
    fn new(from: f64, to: f64) -> Self {
        Self {
            step: if from > 0.0 && to > 0.0 {
                from / to
            } else {
                1.0
            },
            next: 0.0,
            index: 0.0,
            prev: 0.0,
            started: false,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        for &sample in input {
            if !self.started {
                self.prev = sample;
                self.started = true;
                self.index = 0.0;
                self.next = 0.0;
                continue;
            }
            self.index += 1.0;
            // Positions are relative to the previous input sample, so `t` is
            // the fraction of the interval between `prev` and `sample`.
            while self.next <= self.index {
                let t = (self.next - (self.index - 1.0)).clamp(0.0, 1.0) as f32;
                out.push(self.prev + (sample - self.prev) * t);
                self.next += self.step;
            }
            self.prev = sample;
        }
    }
}

pub fn list_input_devices() -> Vec<AudioDeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for device in devices {
        let Ok(name) = device.name() else {
            continue;
        };
        let Ok(config) = device.default_input_config() else {
            continue;
        };
        out.push(AudioDeviceInfo {
            is_default: default_name.as_deref() == Some(name.as_str()),
            name,
            channels: config.channels(),
            sample_rate: config.sample_rate().0,
        });
    }
    out.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Record a short clip to `path`, blocking the calling thread for roughly
/// `seconds`. Used by the settings sheet's "Test recording" control.
///
/// This opens its own stream rather than borrowing the live one: a test while
/// monitoring is running must not inherit the pre-roll buffer or disturb an
/// in-flight segment.
pub fn record_test_clip(device: Option<&str>, seconds: f64, path: &Path) -> Result<f64, String> {
    let host = cpal::default_host();
    let device = match device.map(str::trim).filter(|d| !d.is_empty()) {
        Some(wanted) => host
            .input_devices()
            .map_err(|e| format!("audio_enumerate_failed: {e}"))?
            .find(|d| d.name().ok().as_deref() == Some(wanted))
            .ok_or_else(|| format!("audio_device_not_found: {wanted}"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| "audio_device_not_found: no default input".to_string())?,
    };
    let supported = device
        .default_input_config()
        .map_err(|e| format!("audio_config_unavailable: {e}"))?;
    let config: StreamConfig = supported.config();

    let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
    // `build_stream` already downmixes AND resamples to 16 kHz. Resampling
    // again here divided the clip by the device-to-16k ratio a second time:
    // a 10 s request returned 4 s at 48 kHz, and the audio played slow.
    let sink_tx = tx.clone();
    let sink = move |samples: Vec<f32>| {
        let _ = sink_tx.send(samples);
    };
    let on_error = |message: String| {
        tracing::warn!(error = %message, "test recording stream error");
    };
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, sink, on_error),
        SampleFormat::I16 => build_stream::<i16>(&device, &config, sink, on_error),
        SampleFormat::U16 => build_stream::<u16>(&device, &config, sink, on_error),
        SampleFormat::I32 => build_stream::<i32>(&device, &config, sink, on_error),
        other => return Err(format!("audio_sample_format_unsupported: {other:?}")),
    }
    .map_err(|e| format!("audio_stream_build_failed: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("audio_stream_play_failed: {e}"))?;

    let wanted = (seconds.clamp(0.25, 30.0) * f64::from(RECORDING_SAMPLE_RATE)) as usize;
    let mut collected: Vec<f32> = Vec::with_capacity(wanted);
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(seconds.clamp(0.25, 30.0) + 2.0);
    let mut reason = "deadline";
    while collected.len() < wanted {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(chunk) => {
                collected.extend_from_slice(&chunk);
                if collected.len() >= wanted {
                    collected.truncate(wanted);
                    reason = "complete";
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                reason = "disconnected";
                break;
            }
        }
    }
    drop(stream);
    tracing::info!(
        requested = seconds,
        samples = collected.len(),
        elapsed = started.elapsed().as_secs_f64(),
        reason,
        "test recording finished"
    );

    if collected.is_empty() {
        return Err("audio_test_no_samples: the device produced no audio".to_string());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("wav_create_failed: {e}"))?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: RECORDING_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer =
        hound::WavWriter::create(path, spec).map_err(|e| format!("wav_create_failed: {e}"))?;
    for sample in &collected {
        writer
            .write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .map_err(|e| format!("wav_write_failed: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("wav_finalize_failed: {e}"))?;
    Ok(collected.len() as f64 / f64::from(RECORDING_SAMPLE_RATE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resampler_preserves_duration_within_one_sample() {
        // 48 kHz -> 16 kHz: three input samples per output sample.
        let mut resampler = LinearResampler::new(48_000.0, 16_000.0);
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 / 48_000.0).sin()).collect();
        let mut out = Vec::new();
        resampler.process(&input, &mut out);
        // The interpolation emits one fewer sample than a perfect ratio over the
        // first interval; anything within 0.1% is inaudible and harmless.
        assert!(
            (out.len() as f64 - 16_000.0).abs() < 16.0,
            "got {} samples for one second",
            out.len()
        );
    }

    #[test]
    fn the_resampler_is_a_pass_through_at_the_same_rate() {
        let mut resampler = LinearResampler::new(16_000.0, 16_000.0);
        let mut out = Vec::new();
        resampler.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_full_bus_drops_samples_instead_of_blocking() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let status = Arc::new(CaptureStatus::default());
        let bus = EventBus::new(tx, status.clone());
        for _ in 0..(MAX_QUEUED_CHUNKS + 10) {
            bus.send_samples(vec![0.0; 16]);
        }
        assert_eq!(status.snapshot().dropped_chunks, 10);
    }

    #[test]
    fn silence_is_reported_as_the_floor_not_negative_infinity() {
        let status = CaptureStatus::default();
        status.set_level(0.0);
        assert_eq!(status.snapshot().level_db, -90.0);
        status.set_level(0.5);
        let db = status.snapshot().level_db;
        assert!((db - -6.02).abs() < 0.02, "{db}");
    }
}
