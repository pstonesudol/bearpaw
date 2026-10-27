//! Peak summaries of a stored recording.
//!
//! The recordings list draws a waveform per row, so it needs a cheap shape of
//! the audio — not the audio. Decoding here (rather than in the browser) means
//! the list never downloads a file just to draw bars, and the clipping flag
//! that explains a failed transcript is measured from the samples rather than
//! guessed.

use std::path::Path;

/// One row's waveform, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Waveform {
    /// Peak amplitude per bucket, 0–100. Always `buckets` long.
    pub buckets: Vec<u8>,
    /// Loudest sample in dBFS. `-96.0` for digital silence.
    pub peak_dbfs: f32,
    /// True when a meaningful share of samples sit at full scale. This is what
    /// turns a transcription into `transcription_empty`: a clipped signal is
    /// broadband noise by the time whisper sees it.
    pub clipped: bool,
}

/// Samples this close to full scale count as clipped. One loud transient is not
/// clipping; a signal that lives here is.
const CLIP_SAMPLE_THRESHOLD: i32 = 32_700;
/// Share of samples at the clip threshold that makes the file "clipped".
const CLIP_FRACTION: f64 = 0.002;

/// Summarize a 16-bit PCM WAV.
///
/// Bearpaw writes mono 16 kHz, but this accepts any channel count so an older
/// or hand-placed file still draws something. Multi-channel audio is mixed by
/// taking the loudest channel per frame, which is what the eye expects from a
/// peak meter.
pub(crate) fn summarize(path: &Path, buckets: usize) -> Result<Waveform, String> {
    let buckets = buckets.clamp(8, 512);
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("waveform_unreadable: {e}"))?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(format!(
            "waveform_unsupported_format: {} bit {:?}",
            spec.bits_per_sample, spec.sample_format
        ));
    }
    let channels = usize::from(spec.channels.max(1));
    let total_frames = reader.len() as usize / channels;
    if total_frames == 0 {
        return Ok(Waveform {
            buckets: vec![0; buckets],
            peak_dbfs: -96.0,
            clipped: false,
        });
    }

    let mut peaks = vec![0i32; buckets];
    let mut peak = 0i32;
    let mut at_full_scale = 0usize;
    let frames_per_bucket = (total_frames as f64 / buckets as f64).max(1.0);
    let mut frame = 0usize;
    let mut samples = reader.samples::<i16>();
    'outer: loop {
        // One frame: take the loudest channel.
        let mut frame_peak = 0i32;
        for _ in 0..channels {
            let Some(sample) = samples.next() else {
                break 'outer;
            };
            let value = i32::from(sample.map_err(|e| format!("waveform_unreadable: {e}"))?);
            frame_peak = frame_peak.max(value.saturating_abs());
        }
        let bucket = ((frame as f64) / frames_per_bucket) as usize;
        let bucket = bucket.min(buckets - 1);
        peaks[bucket] = peaks[bucket].max(frame_peak);
        peak = peak.max(frame_peak);
        if frame_peak >= CLIP_SAMPLE_THRESHOLD {
            at_full_scale += 1;
        }
        frame += 1;
    }

    let buckets = peaks
        .into_iter()
        .map(|value| {
            // Round up so a quiet-but-present signal still shows a line rather
            // than reading as silence.
            let scaled = (value as f64 / 32768.0 * 100.0).round() as u8;
            if value > 0 {
                scaled.max(1)
            } else {
                0
            }
        })
        .collect();

    Ok(Waveform {
        buckets,
        peak_dbfs: dbfs(peak),
        clipped: frame > 0 && (at_full_scale as f64 / frame as f64) >= CLIP_FRACTION,
    })
}

fn dbfs(peak: i32) -> f32 {
    if peak <= 0 {
        return -96.0;
    }
    (20.0 * (f64::from(peak) / 32768.0).log10()) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};

    fn write_wav(path: &Path, samples: &[i16]) {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let mut writer = WavWriter::create(path, spec).expect("wav");
        for sample in samples {
            writer.write_sample(*sample).expect("sample");
        }
        writer.finalize().expect("finalize");
    }

    #[test]
    fn a_quiet_then_loud_signal_shapes_the_buckets() {
        let dir = std::env::temp_dir().join(format!("waveform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shape.wav");
        let mut samples = vec![0i16; 8_000];
        samples.extend(vec![16_384i16; 8_000]);
        write_wav(&path, &samples);

        let summary = summarize(&path, 8).expect("summary");
        assert_eq!(summary.buckets.len(), 8);
        assert_eq!(&summary.buckets[..3], &[0, 0, 0]);
        assert!(summary.buckets[4..].iter().all(|b| *b == 50));
        assert!(!summary.clipped);
        assert!((summary.peak_dbfs - -6.02).abs() < 0.1, "{summary:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_signal_pinned_at_full_scale_is_clipped() {
        let dir = std::env::temp_dir().join(format!("waveform-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.wav");
        // A square wave at the rails: what an overdriven line input produces.
        let samples: Vec<i16> = (0..16_000)
            .map(|i| if i % 2 == 0 { 32_767 } else { -32_768 })
            .collect();
        write_wav(&path, &samples);

        let summary = summarize(&path, 16).expect("summary");
        assert!(summary.clipped);
        assert!(summary.peak_dbfs > -0.01, "{summary:?}");
        assert!(summary.buckets.iter().all(|b| *b == 100));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn silence_is_flat_and_quiet() {
        let dir = std::env::temp_dir().join(format!("waveform-silence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("silence.wav");
        write_wav(&path, &vec![0i16; 1_000]);

        let summary = summarize(&path, 8).expect("summary");
        assert!(summary.buckets.iter().all(|b| *b == 0));
        assert_eq!(summary.peak_dbfs, -96.0);
        assert!(!summary.clipped);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_still_returns_buckets() {
        let dir = std::env::temp_dir().join(format!("waveform-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.wav");
        write_wav(&path, &[]);

        let summary = summarize(&path, 8).expect("summary");
        assert_eq!(summary.buckets, vec![0; 8]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
