//! The recording state machine: a pre-roll ring buffer plus one WAV segment at
//! a time.
//!
//! ## Why this is a pure state machine
//!
//! Every rule that can be got wrong — how much pre-roll a segment gets, what a
//! second hit during post-roll does, whether a device loss keeps the bytes it
//! already wrote — is decided here, with no clock and no device. `capture`
//! feeds it frames and events; the worker thread drains [`SegmentOutcome`]s and
//! persists them. That is what makes the timing rules testable on a machine
//! with no microphone.
//!
//! ## Timing comes from sample counts, not wall time
//!
//! Post-roll is "N more samples", not "750 ms from now". A blocked writer
//! cannot stretch a segment, and the duration reported for a file is exactly
//! its length divided by its sample rate. The cost is that a device delivering
//! silence at a different rate than it claims would skew the duration; a real
//! device does not.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};
use serde::Serialize;

/// Whether a finished file is a complete transmission or a partial one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentStatus {
    /// Squelch opened and closed, and the file holds the whole transmission.
    Ready,
    /// The segment ended without a squelch close: device loss, shutdown, or a
    /// frequency change the one receiver cannot express.
    Partial,
}

impl SegmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentStatus::Ready => "ready",
            SegmentStatus::Partial => "partial",
        }
    }
}

/// How sure Bearpaw is that this file belongs to the hit it is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkConfidence {
    /// The squelch open/close pair that delimited the file is the same pair
    /// that delimited the hit row.
    Exact,
    /// Something interrupted the pair — a hit that changed frequency without
    /// closing, a device loss, an overlapping open. The audio is real; which
    /// transmission it belongs to is a guess. The UI says so.
    Uncertain,
}

impl LinkConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkConfidence::Exact => "exact",
            LinkConfidence::Uncertain => "uncertain",
        }
    }
}

/// Why a segment file was thrown away instead of kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    /// Shorter than `min_transmission`, so almost certainly a kerchunk.
    TooShort,
    /// The writer could not create a file at all.
    Unwritable,
}

impl DiscardReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscardReason::TooShort => "too_short",
            DiscardReason::Unwritable => "unwritable",
        }
    }
}

/// What the engine produces for the worker to persist.
#[derive(Clone, Debug, PartialEq)]
pub enum SegmentOutcome {
    Finalized {
        hit_uid: String,
        /// File name relative to the recordings root, never an absolute path.
        file_name: String,
        duration_secs: f64,
        bytes: i64,
        status: SegmentStatus,
        confidence: LinkConfidence,
        error: Option<String>,
    },
    Discarded {
        hit_uid: String,
        reason: DiscardReason,
        /// Recorded length in seconds, for the log line. Not persisted.
        duration_secs: f64,
    },
    Failed {
        hit_uid: String,
        error: String,
    },
}

/// One input to the engine. `Samples` is the only high-volume variant.
#[derive(Clone, Debug, PartialEq)]
pub enum SegmentEvent {
    Samples(Vec<f32>),
    /// The scanner's squelch opened on a real frequency. `hit_uid` is the
    /// durable id the hit row will carry, allocated at open by the poll loop.
    HitOpen {
        hit_uid: String,
    },
    /// Squelch closed. Post-roll starts from here.
    HitClose,
    /// The capture stream died. Any open segment is finished as partial.
    DeviceLost,
    /// Shutdown. Any open segment is finished and flushed.
    Stop,
    /// Apply new timing rules. An open segment keeps the settings it started
    /// with; the next hit uses the new ones.
    Reconfigure(SegmentConfig),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentConfig {
    pub sample_rate: u32,
    /// Audio kept from *before* the squelch opened.
    pub pre_roll: Duration,
    /// Audio kept *after* it closed.
    pub post_roll: Duration,
    /// Segments shorter than this are discarded. Measured from the open, so
    /// pre-roll and post-roll padding cannot rescue a kerchunk.
    pub min_transmission: Duration,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            sample_rate: crate::audio::RECORDING_SAMPLE_RATE,
            pre_roll: Duration::from_millis(500),
            post_roll: Duration::from_millis(750),
            min_transmission: Duration::from_millis(500),
        }
    }
}

impl SegmentConfig {
    fn samples(&self, d: Duration) -> u64 {
        (d.as_secs_f64() * f64::from(self.sample_rate)).round() as u64
    }

    fn pre_roll_samples(&self) -> usize {
        self.samples(self.pre_roll) as usize
    }

    fn post_roll_samples(&self) -> u64 {
        self.samples(self.post_roll)
    }

    fn min_transmission_samples(&self) -> u64 {
        self.samples(self.min_transmission)
    }
}

struct ActiveSegment {
    hit_uid: String,
    part_path: PathBuf,
    final_path: PathBuf,
    writer: Option<WavWriter<BufWriter<File>>>,
    /// Samples in the file so far, including pre-roll and post-roll.
    samples: u64,
    /// Samples received while the squelch was open. This is the only count the
    /// minimum-duration gate may use.
    live_samples: u64,
    /// Post-roll samples still owed. Captured at open so a settings change
    /// mid-transmission cannot mix two rules in one file.
    post_roll_samples: u64,
    post_roll_left: u64,
    /// The minimum-duration gate, also fixed at open.
    min_transmission_samples: u64,
    closing: bool,
    /// Wall-clock start of the post-roll. Only a backstop: normally the sample
    /// count finishes the segment. A device that stops delivering frames after
    /// `HitClose` would otherwise hold a `.part` and a `recording` row open
    /// until the next transmission.
    closing_since: Option<std::time::Instant>,
    confidence: LinkConfidence,
    error: Option<String>,
    write_failed: bool,
}

/// Owns the rolling buffer and at most one open segment.
pub struct SegmentEngine {
    cfg: SegmentConfig,
    dir: PathBuf,
    ring: VecDeque<f32>,
    ring_cap: usize,
    active: Option<ActiveSegment>,
    outcomes: Vec<SegmentOutcome>,
}

impl SegmentEngine {
    pub fn new(cfg: SegmentConfig, dir: impl Into<PathBuf>) -> Self {
        let ring_cap = cfg.pre_roll_samples();
        Self {
            cfg,
            dir: dir.into(),
            ring: VecDeque::with_capacity(ring_cap),
            ring_cap,
            active: None,
            outcomes: Vec::new(),
        }
    }

    /// Replace the timing rules. The ring is resized to the new pre-roll; an
    /// open segment keeps the settings it started with, so a settings change
    /// mid-transmission cannot produce a file with two durations.
    pub fn set_config(&mut self, cfg: SegmentConfig) {
        self.ring_cap = cfg.pre_roll_samples();
        while self.ring.len() > self.ring_cap {
            self.ring.pop_front();
        }
        self.cfg = cfg;
    }

    pub fn config(&self) -> &SegmentConfig {
        &self.cfg
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn handle(&mut self, event: SegmentEvent) {
        match event {
            SegmentEvent::Samples(samples) => self.push_samples(&samples),
            SegmentEvent::HitOpen { hit_uid } => self.open(hit_uid),
            SegmentEvent::HitClose => self.close(),
            SegmentEvent::DeviceLost => self.interrupt("audio_device_lost"),
            SegmentEvent::Stop => self.stop(),
            SegmentEvent::Reconfigure(cfg) => self.set_config(cfg),
        }
    }

    /// Take every outcome produced since the last call.
    pub fn drain(&mut self) -> Vec<SegmentOutcome> {
        std::mem::take(&mut self.outcomes)
    }

    /// Whether a segment is open. Test-only: production asks the outcome
    /// handshake, not the engine, and the engine lives on its own thread.
    #[cfg(test)]
    pub fn is_capturing(&self) -> bool {
        self.active.is_some()
    }

    /// Finish a post-roll that has stopped receiving frames.
    ///
    /// The normal path is sample-driven: post-roll is N more samples. If the
    /// audio device disappears after `HitClose`, those samples never arrive and
    /// the segment would stay open forever. The worker calls this on its idle
    /// timeout; the grace is generous relative to the configured post-roll so
    /// an ordinary callback gap cannot cut a transmission short.
    pub fn force_stale_close(&mut self) {
        let Some(seg) = self.active.as_ref() else {
            return;
        };
        let Some(since) = seg.closing_since else {
            return;
        };
        let limit = self.cfg.post_roll + Duration::from_secs(1);
        if since.elapsed() < limit {
            return;
        }
        if let Some(seg) = self.active.take() {
            self.finish(seg, SegmentStatus::Ready);
        }
    }

    fn push_samples(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }

        if self.ring_cap > 0 {
            for &s in samples {
                if self.ring.len() == self.ring_cap {
                    self.ring.pop_front();
                }
                self.ring.push_back(s);
            }
        }

        let Some(active) = self.active.as_mut() else {
            return;
        };
        let mut write_error = None;
        if let Some(writer) = active.writer.as_mut() {
            for &sample in samples {
                let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                if let Err(e) = writer.write_sample(value) {
                    write_error = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(error) = write_error {
            active.write_failed = true;
            if active.error.is_none() {
                active.error = Some(format!("write_failed: {error}"));
            }
        }

        let written = samples.len() as u64;
        active.samples += written;
        if active.closing {
            active.post_roll_left = active.post_roll_left.saturating_sub(written);
            if active.post_roll_left == 0 {
                let status = SegmentStatus::Ready;
                if let Some(seg) = self.active.take() {
                    self.finish(seg, status);
                }
            }
        } else {
            active.live_samples += written;
        }
    }

    fn open(&mut self, hit_uid: String) {
        // A new open while one is already live is the one-receiver case the
        // feature cannot represent: either the squelch never closed between two
        // transmissions, or the poll loop missed the close. Keep the audio and
        // say the association is uncertain rather than assign it silently.
        if let Some(previous) = self.active.take() {
            let confidence = previous.confidence;
            let carried = previous.error.clone();
            let (status, confidence, error) = if previous.closing {
                // Post-roll cut short by the next transmission. The hit itself
                // was correctly delimited, so this is not uncertainty.
                (SegmentStatus::Ready, confidence, carried)
            } else {
                (
                    SegmentStatus::Partial,
                    LinkConfidence::Uncertain,
                    carried.or_else(|| Some("superseded_without_squelch_close".to_string())),
                )
            };
            self.finish_with(previous, status, confidence, error);
        }

        let file_name = format!("{}.wav", sanitize_uid(&hit_uid));
        let final_path = self.dir.join(&file_name);
        let part_path = self.dir.join(format!("{file_name}.part"));

        if let Err(e) = fs::create_dir_all(&self.dir) {
            self.outcomes.push(SegmentOutcome::Failed {
                hit_uid,
                error: format!("recordings_dir_unwritable: {e}"),
            });
            return;
        }

        let spec = WavSpec {
            channels: 1,
            sample_rate: self.cfg.sample_rate,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = match WavWriter::create(&part_path, spec) {
            Ok(w) => w,
            Err(e) => {
                self.outcomes.push(SegmentOutcome::Failed {
                    hit_uid,
                    error: format!("wav_create_failed: {e}"),
                });
                return;
            }
        };

        let mut active = ActiveSegment {
            hit_uid,
            part_path,
            final_path,
            writer: Some(writer),
            samples: 0,
            live_samples: 0,
            post_roll_samples: self.cfg.post_roll_samples(),
            post_roll_left: 0,
            min_transmission_samples: self.cfg.min_transmission_samples(),
            closing: false,
            closing_since: None,
            confidence: LinkConfidence::Exact,
            error: None,
            write_failed: false,
        };

        // Pre-roll: everything the rolling buffer holds, oldest first.
        if let Some(writer) = active.writer.as_mut() {
            for &sample in &self.ring {
                let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                if writer.write_sample(value).is_err() {
                    active.write_failed = true;
                    active.error = Some("write_failed".to_string());
                    break;
                }
            }
        }
        active.samples = self.ring.len() as u64;
        self.active = Some(active);
    }

    fn close(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.closing {
            return;
        }
        active.closing = true;
        active.closing_since = Some(std::time::Instant::now());
        active.post_roll_left = active.post_roll_samples;
        if active.post_roll_left == 0 {
            if let Some(seg) = self.active.take() {
                self.finish(seg, SegmentStatus::Ready);
            }
        }
    }

    fn interrupt(&mut self, error: &str) {
        let Some(mut seg) = self.active.take() else {
            return;
        };
        if seg.closing {
            // A clean close was already seen; only the post-roll tail is
            // missing, and post-roll is padding rather than transmission. The
            // file and its hit association are complete.
            self.finish(seg, SegmentStatus::Ready);
            return;
        }
        seg.confidence = LinkConfidence::Uncertain;
        if seg.error.is_none() {
            seg.error = Some(error.to_string());
        }
        let carried = seg.error.clone();
        self.finish_with(
            seg,
            SegmentStatus::Partial,
            LinkConfidence::Uncertain,
            carried,
        );
    }

    fn stop(&mut self) {
        let Some(mut seg) = self.active.take() else {
            return;
        };
        if seg.closing {
            self.finish(seg, SegmentStatus::Ready);
            return;
        }
        seg.confidence = LinkConfidence::Uncertain;
        if seg.error.is_none() {
            seg.error = Some("recording_stopped".to_string());
        }
        let carried = seg.error.clone();
        self.finish_with(
            seg,
            SegmentStatus::Partial,
            LinkConfidence::Uncertain,
            carried,
        );
    }

    fn finish(&mut self, seg: ActiveSegment, status: SegmentStatus) {
        let confidence = seg.confidence;
        let error = seg.error.clone();
        self.finish_with(seg, status, confidence, error);
    }

    fn finish_with(
        &mut self,
        mut seg: ActiveSegment,
        status: SegmentStatus,
        confidence: LinkConfidence,
        error: Option<String>,
    ) {
        let hit_uid = seg.hit_uid.clone();
        let samples = seg.samples;
        let live_samples = seg.live_samples;
        let duration_secs = samples as f64 / f64::from(self.cfg.sample_rate);
        let write_failed = seg.write_failed;

        // Drop the writer before touching the file: Windows refuses to delete a
        // path with an open handle, and `finalize` is what writes the WAV
        // trailer. A finalize failure is a failed recording, not a silent one.
        let finalize_error = match seg.writer.take().map(WavWriter::finalize) {
            Some(Err(e)) => Some(format!("wav_finalize_failed: {e}")),
            _ => None,
        };

        if write_failed || finalize_error.is_some() {
            let _ = fs::remove_file(&seg.part_path);
            self.outcomes.push(SegmentOutcome::Failed {
                hit_uid,
                error: error
                    .or(finalize_error)
                    .unwrap_or_else(|| "wav_write_failed".to_string()),
            });
            return;
        }

        if live_samples < seg.min_transmission_samples {
            let _ = fs::remove_file(&seg.part_path);
            self.outcomes.push(SegmentOutcome::Discarded {
                hit_uid,
                reason: DiscardReason::TooShort,
                duration_secs,
            });
            return;
        }

        if let Err(e) = fs::rename(&seg.part_path, &seg.final_path) {
            let _ = fs::remove_file(&seg.part_path);
            self.outcomes.push(SegmentOutcome::Failed {
                hit_uid,
                error: format!("wav_rename_failed: {e}"),
            });
            return;
        }

        // File name, not path: the recordings root is a preference and may move.
        let file_name = seg
            .final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.outcomes.push(SegmentOutcome::Finalized {
            hit_uid,
            file_name,
            duration_secs,
            bytes: (samples * 2) as i64,
            status,
            confidence,
            error: error.filter(|_| status == SegmentStatus::Partial),
        });
    }
}

/// File names come from generated hit ids, not user input, but a path
/// separator here would be a traversal bug, so refuse to construct one.
fn sanitize_uid(uid: &str) -> String {
    let cleaned: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "hit".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 8_000;

    fn cfg() -> SegmentConfig {
        SegmentConfig {
            sample_rate: RATE,
            pre_roll: Duration::from_millis(100),
            post_roll: Duration::from_millis(200),
            min_transmission: Duration::from_millis(100),
        }
    }

    fn engine() -> (SegmentEngine, tempdir::Dir) {
        let dir = tempdir::Dir::new();
        (SegmentEngine::new(cfg(), dir.path()), dir)
    }

    /// Minimal temp-directory helper. The repo takes no dev-dependency for one;
    /// a PID-and-counter directory under the OS temp root is enough and is left
    /// for the OS to reclaim, matching `unique_test_db_path`.
    mod tempdir {
        use std::path::{Path, PathBuf};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                static N: AtomicU64 = AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "bearpaw-audio-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("temp dir");
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn constant(n: usize, value: f32) -> Vec<f32> {
        vec![value; n]
    }

    fn outcomes(engine: &mut SegmentEngine) -> Vec<SegmentOutcome> {
        engine.drain()
    }

    fn finalized(outcomes: &[SegmentOutcome]) -> &SegmentOutcome {
        outcomes
            .iter()
            .find(|o| matches!(o, SegmentOutcome::Finalized { .. }))
            .expect("a finalized recording")
    }

    #[test]
    fn pre_roll_from_before_the_open_is_included() {
        let (mut e, dir) = engine();
        // 80 ms of pre-roll at 8 kHz.
        e.handle(SegmentEvent::Samples(constant(640, 0.25)));
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "h1".into(),
        });
        // 300 ms of transmission.
        e.handle(SegmentEvent::Samples(constant(2_400, 0.5)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(1_600, 0.0)));

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized {
            file_name,
            duration_secs,
            bytes,
            status,
            confidence,
            ..
        } = finalized(&out)
        else {
            unreachable!()
        };
        assert_eq!(file_name, "h1.wav");
        assert_eq!(*status, SegmentStatus::Ready);
        assert_eq!(*confidence, LinkConfidence::Exact);
        // 640 pre-roll + 2400 live + 1600 post-roll = 4640 samples @ 8 kHz.
        assert!((*duration_secs - 0.58).abs() < 1e-9, "{duration_secs}");
        assert_eq!(*bytes, 4640 * 2);
        assert!(dir.path().join("h1.wav").exists());
        assert!(!dir.path().join("h1.wav.part").exists());
    }

    #[test]
    fn post_roll_is_cut_short_by_the_next_transmission_without_uncertainty() {
        let (mut e, dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "first".into(),
        });
        e.handle(SegmentEvent::Samples(constant(800, 0.5)));
        e.handle(SegmentEvent::HitClose);
        // Only half the post-roll arrives before the next hit.
        e.handle(SegmentEvent::Samples(constant(800, 0.0)));
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "second".into(),
        });

        let out = outcomes(&mut e);
        assert_eq!(out.len(), 1);
        let SegmentOutcome::Finalized {
            hit_uid,
            status,
            confidence,
            ..
        } = &out[0]
        else {
            panic!("expected finalized, got {out:?}")
        };
        assert_eq!(hit_uid, "first");
        assert_eq!(*status, SegmentStatus::Ready);
        assert_eq!(*confidence, LinkConfidence::Exact);
        assert!(dir.path().join("first.wav").exists());
        assert!(e.is_capturing(), "the second transmission is recording");
    }

    #[test]
    fn an_open_without_a_close_is_kept_as_uncertain() {
        let (mut e, _dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "old".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.5)));
        // No HitClose: this is the frequency-changed-mid-squelch case.
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "new".into(),
        });

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized {
            hit_uid,
            status,
            confidence,
            error,
            ..
        } = &out[0]
        else {
            panic!("expected finalized")
        };
        assert_eq!(hit_uid, "old");
        assert_eq!(*status, SegmentStatus::Partial);
        assert_eq!(*confidence, LinkConfidence::Uncertain);
        assert_eq!(error.as_deref(), Some("superseded_without_squelch_close"));
    }

    #[test]
    fn a_kerchunk_is_discarded_but_the_ring_survives() {
        let (mut e, dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "blip".into(),
        });
        // 50 ms live, below the 100 ms minimum, plus 200 ms post-roll.
        e.handle(SegmentEvent::Samples(constant(400, 0.9)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(1_600, 0.0)));

        let out = outcomes(&mut e);
        assert!(matches!(
            out.as_slice(),
            [SegmentOutcome::Discarded {
                reason: DiscardReason::TooShort,
                ..
            }]
        ));
        assert!(!dir.path().join("blip.wav").exists());
        assert!(!dir.path().join("blip.wav.part").exists());
    }

    #[test]
    fn pre_roll_alone_cannot_rescue_a_segment_below_the_minimum() {
        let (mut e, _dir) = engine();
        // A full pre-roll, then 10 ms of live audio. The minimum is measured
        // from the open so padding cannot make a kerchunk look like a
        // transmission.
        e.handle(SegmentEvent::Samples(constant(800, 0.2)));
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "short".into(),
        });
        e.handle(SegmentEvent::Samples(constant(80, 0.9)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(1_600, 0.0)));

        assert!(matches!(
            outcomes(&mut e).as_slice(),
            [SegmentOutcome::Discarded { .. }]
        ));
    }

    #[test]
    fn the_ring_is_bounded_by_the_pre_roll() {
        let (mut e, _dir) = engine();
        // 5 seconds of audio into a 100 ms ring.
        e.handle(SegmentEvent::Samples(constant(RATE as usize * 5, 0.1)));
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "bounded".into(),
        });
        // 300 ms live.
        e.handle(SegmentEvent::Samples(constant(2_400, 0.5)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(1_600, 0.0)));

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized { duration_secs, .. } = finalized(&out) else {
            unreachable!()
        };
        // 100 ms pre-roll + 300 ms live + 200 ms post-roll, not five seconds.
        assert!((*duration_secs - 0.6).abs() < 1e-9, "{duration_secs}");
    }

    #[test]
    fn a_device_loss_finishes_the_open_segment_as_partial() {
        let (mut e, dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "lost".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.4)));
        e.handle(SegmentEvent::DeviceLost);

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized {
            status,
            confidence,
            error,
            ..
        } = finalized(&out)
        else {
            unreachable!()
        };
        assert_eq!(*status, SegmentStatus::Partial);
        assert_eq!(*confidence, LinkConfidence::Uncertain);
        assert_eq!(error.as_deref(), Some("audio_device_lost"));
        assert!(dir.path().join("lost.wav").exists());
        assert!(!e.is_capturing());
    }

    #[test]
    fn a_device_loss_during_post_roll_keeps_a_complete_file() {
        let (mut e, _dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "closed".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.4)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(400, 0.0)));
        e.handle(SegmentEvent::DeviceLost);

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized {
            status, confidence, ..
        } = finalized(&out)
        else {
            unreachable!()
        };
        assert_eq!(*status, SegmentStatus::Ready);
        assert_eq!(*confidence, LinkConfidence::Exact);
    }

    #[test]
    fn a_close_with_no_open_is_ignored() {
        let (mut e, _dir) = engine();
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(800, 0.1)));
        assert!(outcomes(&mut e).is_empty());
        assert!(!e.is_capturing());
    }

    #[test]
    fn zero_post_roll_finishes_on_the_close() {
        let (mut e, _dir) = engine();
        e.set_config(SegmentConfig {
            post_roll: Duration::ZERO,
            ..cfg()
        });
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "instant".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.5)));
        e.handle(SegmentEvent::HitClose);
        assert!(matches!(
            outcomes(&mut e).as_slice(),
            [SegmentOutcome::Finalized {
                status: SegmentStatus::Ready,
                ..
            }]
        ));
    }

    #[test]
    fn shutdown_flushes_an_open_segment() {
        let (mut e, dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "shutdown".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.5)));
        e.handle(SegmentEvent::Stop);
        assert!(dir.path().join("shutdown.wav").exists());
        assert!(!e.is_capturing());
    }

    #[test]
    fn an_unwritable_directory_is_a_failure_not_a_panic() {
        let mut e = SegmentEngine::new(cfg(), "/dev/null/not-a-directory");
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "nope".into(),
        });
        assert!(matches!(
            outcomes(&mut e).as_slice(),
            [SegmentOutcome::Failed { .. }]
        ));
        assert!(!e.is_capturing());
    }

    #[test]
    fn hit_ids_cannot_escape_the_recordings_directory() {
        let (mut e, dir) = engine();
        e.handle(SegmentEvent::HitOpen {
            hit_uid: "../../etc/passwd".into(),
        });
        e.handle(SegmentEvent::Samples(constant(1_600, 0.5)));
        e.handle(SegmentEvent::HitClose);
        e.handle(SegmentEvent::Samples(constant(1_600, 0.0)));

        let out = outcomes(&mut e);
        let SegmentOutcome::Finalized { file_name, .. } = finalized(&out) else {
            unreachable!()
        };
        assert!(!file_name.contains('/'));
        assert!(dir.path().join(file_name).exists());
    }
}
