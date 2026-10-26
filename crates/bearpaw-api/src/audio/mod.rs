//! Audio capture and hit-recording segmentation.
//!
//! The split here is deliberate:
//!
//! - [`segment`] is a pure state machine over frames and hit events. It owns the
//!   pre-roll ring buffer and the WAV files, takes no clock from the system, and
//!   is where every recording rule is tested.
//! - [`capture`] is the only module that talks to CoreAudio (through `cpal`). It
//!   converts device frames to mono 16 kHz and pushes them at the segmenter.
//!
//! Nothing here touches the scanner, and nothing here runs on the poll thread.
//! A stalled disk or a vanished audio device can delay a recording and nothing
//! else.

pub mod capture;
pub mod segment;

pub use capture::{
    capture_status, list_input_devices, AudioDeviceInfo, CaptureCommand, CaptureConfig,
    CaptureHandle, CaptureStatus, CaptureStatusSnapshot, EventBus,
};
pub use segment::{
    LinkConfidence, SegmentConfig, SegmentEngine, SegmentEvent, SegmentOutcome, SegmentStatus,
};

/// Every recording is stored at this rate, mono, 16-bit PCM.
///
/// 16 kHz is whisper's native input rate and is more than enough for narrowband
/// radio. Storing the device's native rate would cost disk and force a
/// resample at transcription time; storing lower would throw away speech.
pub const RECORDING_SAMPLE_RATE: u32 = 16_000;

/// How long a single `cpal` callback's converted samples may sit in the queue
/// before further frames are dropped.
///
/// Generous on purpose: a transient disk stall of a second or two should not
/// punch a hole in a recording. Past this the memory cost (a few MB) is worse
/// than the gap, and a gap is visible in the file rather than silent.
pub const MAX_QUEUED_CHUNKS: usize = 512;
