//! The recording service: owns the capture thread, the segmenter thread, the
//! recordings directory, and the bridge to the analytics database and the
//! transcription queue.
//!
//! Nothing in here is on the scanner's critical path. `hit_open`/`hit_close`
//! only push an event into a channel; a stalled disk, a dead audio device, or a
//! database that is temporarily locked can delay a recording and nothing else.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

use crate::api::hit_store;
use crate::audio::capture::{
    self, AudioDeviceInfo, CaptureConfig, CaptureHandle, CaptureStatus, CaptureStatusSnapshot,
    EventBus,
};
use crate::audio::segment::{
    LinkConfidence, SegmentConfig, SegmentEngine, SegmentEvent, SegmentOutcome, SegmentStatus,
};
use crate::transcription::TranscriptionService;

#[derive(Clone, Debug)]
pub struct RecordingSettings {
    pub enabled: bool,
    /// Exact CoreAudio input name; `None` means the system default.
    pub device: Option<String>,
    pub pre_roll: Duration,
    pub post_roll: Duration,
    pub min_transmission: Duration,
    /// `0` keeps audio until the hit itself ages out.
    pub retention_days: u32,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            device: None,
            pre_roll: Duration::from_millis(500),
            post_roll: Duration::from_millis(750),
            min_transmission: Duration::from_millis(500),
            retention_days: 30,
        }
    }
}

impl RecordingSettings {
    /// Read the audio settings and resolve the recordings root.
    ///
    /// The root comes from `audio_storage_dir` when set, otherwise
    /// `<data dir>/recordings`. Resolving it here keeps one answer to "where do
    /// files live" instead of each caller joining paths differently.
    pub fn from_preferences(prefs: &Map<String, Value>, data_dir: &Path) -> (Self, PathBuf) {
        let millis = |key: &str, fallback: u64, max: u64| {
            Duration::from_millis(
                prefs
                    .get(key)
                    .and_then(Value::as_u64)
                    .unwrap_or(fallback)
                    .min(max),
            )
        };
        let enabled = prefs
            .get("audio_recording_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let device = prefs
            .get("audio_input_device")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let root = prefs
            .get("audio_storage_dir")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("recordings"));
        let retention_days = prefs
            .get("audio_retention_days")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .min(3650) as u32;
        (
            Self {
                enabled,
                device,
                pre_roll: millis("audio_pre_roll_ms", 500, 5_000),
                post_roll: millis("audio_post_roll_ms", 750, 10_000),
                min_transmission: millis("audio_min_duration_ms", 500, 10_000),
                retention_days,
            },
            root,
        )
    }

    fn segment_config(&self) -> SegmentConfig {
        SegmentConfig {
            sample_rate: crate::audio::RECORDING_SAMPLE_RATE,
            pre_roll: self.pre_roll,
            post_roll: self.post_roll,
            min_transmission: self.min_transmission,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecordingStatus {
    pub enabled: bool,
    pub active_hit: bool,
    pub recordings_dir: String,
    pub retention_days: u32,
    pub capture: CaptureStatusSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct TestClip {
    pub file: String,
    pub duration_secs: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SweepReport {
    pub partial_files_removed: usize,
    pub interrupted_rows: usize,
    pub missing_rows: usize,
    pub orphan_files_removed: usize,
    pub retention_files_removed: usize,
    pub retention_rows_marked: usize,
}

#[derive(Default)]
struct ServiceInner {
    bus: Option<EventBus>,
    capture: Option<CaptureHandle>,
    worker: Option<JoinHandle<()>>,
}

pub struct RecordingService {
    inner: Mutex<ServiceInner>,
    status: Arc<CaptureStatus>,
    db_path: Arc<String>,
    ws_tx: broadcast::Sender<String>,
    transcription: Arc<TranscriptionService>,
    root: Arc<Mutex<PathBuf>>,
    settings: Mutex<RecordingSettings>,
    active_uid: Mutex<Option<String>>,
}

impl RecordingService {
    pub fn new(
        db_path: Arc<String>,
        ws_tx: broadcast::Sender<String>,
        transcription: Arc<TranscriptionService>,
        settings: RecordingSettings,
        root: Arc<Mutex<PathBuf>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ServiceInner::default()),
            status: Arc::new(CaptureStatus::default()),
            db_path,
            ws_tx,
            transcription,
            root,
            settings: Mutex::new(settings),
            active_uid: Mutex::new(None),
        })
    }

    pub fn settings(&self) -> RecordingSettings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn root(&self) -> PathBuf {
        self.root
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|_| std::env::temp_dir().join("bearpaw-recordings"))
    }

    /// Apply new settings, starting or stopping capture as needed.
    ///
    /// A device change restarts the capture thread. Timing changes are sent to
    /// the segmenter and take effect on the next transmission; a file already
    /// being written keeps the settings it started with.
    pub fn apply_settings(&self, settings: RecordingSettings, root: PathBuf) {
        let previous = self.settings();
        let device_changed = previous.device != settings.device;
        if let Ok(mut slot) = self.settings.lock() {
            *slot = settings.clone();
        }
        if let Ok(mut slot) = self.root.lock() {
            *slot = root;
        }

        if !settings.enabled {
            self.stop();
            return;
        }

        let running = self
            .inner
            .lock()
            .map(|inner| inner.capture.is_some())
            .unwrap_or(false);
        if running && device_changed {
            self.stop();
        }
        self.start();
        if let Some(bus) = self.bus() {
            bus.send(SegmentEvent::Reconfigure(settings.segment_config()));
        }
    }

    pub fn start(&self) {
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if inner.capture.is_some() {
            return;
        }
        let settings = self.settings();
        if !settings.enabled {
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let bus = EventBus::new(tx, self.status.clone());
        let engine = SegmentEngine::new(settings.segment_config(), self.root());
        let sink = RecordingSink {
            db_path: self.db_path.clone(),
            ws_tx: self.ws_tx.clone(),
            transcription: self.transcription.clone(),
            root: self.root.clone(),
        };
        let worker_bus = bus.clone();
        let worker = thread::Builder::new()
            .name("bearpaw-audio-segmenter".to_string())
            .spawn(move || segment_worker(rx, engine, sink, worker_bus))
            .ok();

        let capture = capture::spawn(
            CaptureConfig {
                device: settings.device.clone(),
            },
            bus.clone(),
            self.status.clone(),
        );

        inner.bus = Some(bus);
        inner.capture = Some(capture);
        inner.worker = worker;
    }

    pub fn stop(&self) {
        let (bus, capture, worker) = {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            (inner.bus.take(), inner.capture.take(), inner.worker.take())
        };
        if let Some(bus) = &bus {
            bus.send(SegmentEvent::Stop);
        }
        if let Some(mut capture) = capture {
            capture.stop();
        }
        if let Some(worker) = worker {
            let _ = worker.join();
        }
        if let Ok(mut uid) = self.active_uid.lock() {
            *uid = None;
        }
    }

    fn bus(&self) -> Option<EventBus> {
        self.inner.lock().ok().and_then(|inner| inner.bus.clone())
    }

    pub fn hit_open(&self, hit_uid: &str) {
        if !self.settings().enabled {
            return;
        }
        if let Ok(mut slot) = self.active_uid.lock() {
            *slot = Some(hit_uid.to_string());
        }
        if let Some(bus) = self.bus() {
            bus.send(SegmentEvent::HitOpen {
                hit_uid: hit_uid.to_string(),
            });
        }
    }

    pub fn hit_close(&self) {
        if let Ok(mut slot) = self.active_uid.lock() {
            *slot = None;
        }
        if let Some(bus) = self.bus() {
            bus.send(SegmentEvent::HitClose);
        }
    }

    /// The `recording_status` a new hit row should carry.
    ///
    /// Called at hit-close, before `hit_close` clears the active uid: if the
    /// segmenter is holding a segment for this hit, the row starts life as
    /// `recording` and the worker updates it. If recording is off, the row is
    /// `none` and no worker will ever touch it.
    pub fn initial_status(&self, hit_uid: &str) -> &'static str {
        let active = self.active_uid.lock().ok().and_then(|slot| slot.clone());
        if active.as_deref() == Some(hit_uid) {
            "recording"
        } else {
            "none"
        }
    }

    pub fn status(&self) -> RecordingStatus {
        RecordingStatus {
            enabled: self.settings().enabled,
            active_hit: self
                .active_uid
                .lock()
                .map(|slot| slot.is_some())
                .unwrap_or(false),
            recordings_dir: self.root().to_string_lossy().into_owned(),
            retention_days: self.settings().retention_days,
            capture: self.status.snapshot(),
        }
    }

    pub fn list_devices(&self) -> Vec<AudioDeviceInfo> {
        capture::list_input_devices()
    }

    /// Record a short clip with its own stream, so a test cannot disturb an
    /// in-flight hit. Blocking for the length of the clip; callers run it on a
    /// blocking task.
    pub fn test_clip(&self, seconds: f64) -> Result<TestClip, String> {
        let dir = self.root().join("_tests");
        let name = format!("test-{}.wav", crate::api::uuid_simple());
        let path = dir.join(&name);
        let device = self.settings().device;
        let duration = capture::record_test_clip(device.as_deref(), seconds, &path)?;
        Ok(TestClip {
            file: format!("_tests/{name}"),
            duration_secs: duration,
        })
    }

    /// Resolve a stored file name under the recordings root.
    pub fn audio_path(&self, file_name: &str) -> Option<PathBuf> {
        hit_store::safe_join(&self.root(), file_name)
    }

    pub fn delete_audio_file(&self, file_name: &str) -> bool {
        self.audio_path(file_name)
            .and_then(|path| std::fs::remove_file(path).ok())
            .is_some()
    }

    /// Startup and daily housekeeping: interrupted rows, missing files, stale
    /// `.part` files, orphans and retention. Idempotent; safe to call at any
    /// time, including while recording.
    pub fn reconcile(&self) -> SweepReport {
        let root = self.root();
        let mut report = SweepReport::default();

        // 1. Half-written files are never a recording. A crash between create
        //    and rename leaves one; the row that would have pointed at it is
        //    reconciled below.
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("part") {
                    if std::fs::remove_file(&path).is_ok() {
                        report.partial_files_removed += 1;
                    }
                }
            }
        }

        // 2. A row still marked `recording`/`pending` was interrupted by a
        //    crash, a power loss, or a device error that never delivered an
        //    outcome. Recover the bytes if they exist.
        for (hit_uid, recorded_path) in hit_store::interrupted_recordings(&self.db_path) {
            // A row that never got a path still has a predictable file name:
            // the segmenter names every file after the hit uid.
            let file = recorded_path.unwrap_or_else(|| format!("{hit_uid}.wav"));
            let path = self.audio_path(&file);
            let exists = path.as_ref().is_some_and(|p| p.is_file());
            report.interrupted_rows += 1;
            if exists {
                hit_store::set_recording_outcome(
                    &self.db_path,
                    &hit_uid,
                    Some(&file),
                    "partial",
                    Some(LinkConfidence::Uncertain.as_str()),
                    file_size_secs(path.as_deref()),
                    path.as_ref()
                        .and_then(|p| std::fs::metadata(p).ok())
                        .map(|m| m.len() as i64),
                    Some("recording_interrupted"),
                );
            } else {
                hit_store::set_recording_outcome(
                    &self.db_path,
                    &hit_uid,
                    None,
                    "failed",
                    None,
                    None,
                    None,
                    Some("recording_interrupted"),
                );
            }
        }

        // 3. A row that points at a file which is not there is `missing`, not
        //    `ready`. The hit and transcript stay; the UI stops offering
        //    playback.
        for (hit_uid, file, status, _) in hit_store::recording_rows(&self.db_path) {
            if matches!(status.as_str(), "ready" | "partial") {
                let exists = self.audio_path(&file).is_some_and(|p| p.is_file());
                if !exists {
                    hit_store::set_recording_missing(&self.db_path, &hit_uid);
                    report.missing_rows += 1;
                }
            }
        }

        // 4. Retention, then orphans. Orphans are deleted only after a day so a
        //    file whose row is still being written is never swept out from
        //    under it.
        let (files, rows) =
            hit_store::apply_audio_retention(&self.db_path, &root, self.settings().retention_days);
        report.retention_files_removed = files;
        report.retention_rows_marked = rows;

        let referenced: std::collections::HashSet<String> =
            hit_store::recording_rows(&self.db_path)
                .into_iter()
                .map(|(_, file, _, _)| file)
                .collect();
        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(24 * 3600))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                if path.extension().and_then(|e| e.to_str()) == Some("part") {
                    continue;
                }
                let test_clip = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|p| p == "_tests");
                let referenced_elsewhere = referenced.contains(&name)
                    || (test_clip
                        && referenced
                            .iter()
                            .any(|r| r.ends_with(&format!("_tests/{name}"))));
                if referenced_elsewhere {
                    continue;
                }
                let old_enough = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|modified| modified < cutoff)
                    .unwrap_or(false);
                if old_enough && std::fs::remove_file(&path).is_ok() {
                    report.orphan_files_removed += 1;
                }
            }
        }

        report
    }
}

impl Drop for RecordingService {
    fn drop(&mut self) {
        // The service is in an Arc in AppState, so this normally runs at
        // process exit. `stop` is explicit in the shutdown path; this is the
        // belt for tests and for a panic that unwinds AppState.
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(bus) = inner.bus.take() {
                bus.send(SegmentEvent::Stop);
            }
            if let Some(mut capture) = inner.capture.take() {
                capture.stop();
            }
            if let Some(worker) = inner.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

fn file_size_secs(path: Option<&Path>) -> Option<f64> {
    let path = path?;
    let bytes = std::fs::metadata(path).ok()?.len();
    // 16-bit mono at 16 kHz: 32,000 bytes per second, minus the 44-byte header.
    Some(bytes.saturating_sub(44) as f64 / 32_000.0)
}

struct RecordingSink {
    db_path: Arc<String>,
    ws_tx: broadcast::Sender<String>,
    transcription: Arc<TranscriptionService>,
    root: Arc<Mutex<PathBuf>>,
}

impl RecordingSink {
    fn persist(&self, outcome: SegmentOutcome) {
        match outcome {
            SegmentOutcome::Finalized {
                hit_uid,
                file_name,
                duration_secs,
                bytes,
                status,
                confidence,
                error,
            } => {
                let stored = update_with_retry(|| {
                    hit_store::set_recording_outcome(
                        &self.db_path,
                        &hit_uid,
                        Some(&file_name),
                        status.as_str(),
                        Some(confidence.as_str()),
                        Some(duration_secs),
                        Some(bytes),
                        error.as_deref(),
                    )
                });
                if !stored {
                    // No hit row: the transmission was below `hit_min_duration`
                    // or the database went away. Keeping the file would leave
                    // an orphan with no history to belong to.
                    self.delete_file(&file_name);
                    return;
                }
                self.broadcast(
                    &hit_uid,
                    status.as_str(),
                    Some(duration_secs),
                    Some(confidence.as_str()),
                    error.as_deref(),
                );
                if matches!(status, SegmentStatus::Ready | SegmentStatus::Partial) {
                    self.transcription.enqueue(&hit_uid);
                }
            }
            SegmentOutcome::Discarded {
                hit_uid,
                reason,
                duration_secs,
            } => {
                hit_store::set_recording_outcome(
                    &self.db_path,
                    &hit_uid,
                    None,
                    reason.as_str(),
                    None,
                    Some(duration_secs),
                    None,
                    None,
                );
                self.broadcast(&hit_uid, reason.as_str(), Some(duration_secs), None, None);
            }
            SegmentOutcome::Failed { hit_uid, error } => {
                tracing::warn!(hit_uid, error, "recording failed");
                hit_store::set_recording_outcome(
                    &self.db_path,
                    &hit_uid,
                    None,
                    "failed",
                    None,
                    None,
                    None,
                    Some(&error),
                );
                self.broadcast(&hit_uid, "failed", None, None, Some(&error));
            }
        }
    }

    fn delete_file(&self, file_name: &str) {
        let root = self.root.lock().map(|r| r.clone()).unwrap_or_default();
        if let Some(path) = hit_store::safe_join(&root, file_name) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn broadcast(
        &self,
        hit_uid: &str,
        status: &str,
        duration_secs: Option<f64>,
        confidence: Option<&str>,
        error: Option<&str>,
    ) {
        let _ = self.ws_tx.send(
            json!({
                "type": "event",
                "timestamp": crate::api::epoch_now(),
                "event": "recording_ready",
                "data": {
                    "hit_uid": hit_uid,
                    "recording_status": status,
                    "recording_duration": duration_secs,
                    "link_confidence": confidence,
                    "recording_error": error,
                }
            })
            .to_string(),
        );
    }
}

/// The insert that creates the hit row runs on the poll thread immediately
/// before `HitClose` is queued, so a missing row is usually just that ordering.
/// A short retry covers it without holding the segmenter for long.
fn update_with_retry(mut attempt: impl FnMut() -> bool) -> bool {
    for _ in 0..5 {
        if attempt() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn segment_worker(
    rx: Receiver<SegmentEvent>,
    mut engine: SegmentEngine,
    sink: RecordingSink,
    bus: EventBus,
) {
    while let Ok(event) = rx.recv() {
        let is_samples = matches!(event, SegmentEvent::Samples(_));
        let is_stop = matches!(event, SegmentEvent::Stop);
        engine.handle(event);
        for outcome in engine.drain() {
            sink.persist(outcome);
        }
        if is_samples {
            bus.completed_one();
        }
        if is_stop {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn prefs(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn recording_is_off_and_follows_hits_until_the_user_opts_in() {
        let (settings, root) = RecordingSettings::from_preferences(
            &prefs(json!({"audio_recording_enabled": true})),
            Path::new("/data"),
        );
        assert!(settings.enabled);
        assert_eq!(root, PathBuf::from("/data/recordings"));

        let (default, _) =
            RecordingSettings::from_preferences(&prefs(json!({})), Path::new("/data"));
        assert!(!default.enabled, "recording must be opt-in");
    }

    #[test]
    fn timing_settings_are_clamped_to_sane_values() {
        let (settings, _) = RecordingSettings::from_preferences(
            &prefs(json!({
                "audio_pre_roll_ms": 99_999,
                "audio_post_roll_ms": 99_999,
                "audio_min_duration_ms": 99_999,
            })),
            Path::new("/data"),
        );
        assert_eq!(settings.pre_roll, Duration::from_millis(5_000));
        assert_eq!(settings.post_roll, Duration::from_millis(10_000));
        assert_eq!(settings.min_transmission, Duration::from_millis(10_000));
    }

    #[test]
    fn a_custom_storage_directory_wins() {
        let (_, root) = RecordingSettings::from_preferences(
            &prefs(json!({"audio_storage_dir": "/Volumes/Radio/audio"})),
            Path::new("/data"),
        );
        assert_eq!(root, PathBuf::from("/Volumes/Radio/audio"));
    }

    use crate::api::{init_analytics_db, insert_analytics_hit, unique_test_db_path, ActivityHit};
    use crate::state::ScannerMode;

    fn seeded_hit(db: &str, uid: &str, status: &str, file: Option<&str>) {
        insert_analytics_hit(
            db,
            &ActivityHit {
                id: "1".to_string(),
                timestamp: crate::api::epoch_now(),
                frequency: 160.725,
                channel: Some(12),
                alpha_tag: Some("NORFOLK SOUTHERN".to_string()),
                rssi: 40,
                duration: 4.0,
                modulation: "NFM".to_string(),
                mode: ScannerMode::Scan,
                bank: Some(3),
                session_id: "s".to_string(),
                ended_at: crate::api::epoch_now() + 4.0,
                scanner_id: Some("BC125AT".to_string()),
                hit_uid: Some(uid.to_string()),
                recording_status: status.to_string(),
                recording_file: file.map(str::to_string),
                ..Default::default()
            },
        );
    }

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "bearpaw-recordings-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn sink(db: &str, root: &PathBuf, ws: &broadcast::Sender<String>) -> RecordingSink {
        let root = Arc::new(Mutex::new(root.clone()));
        let transcription = TranscriptionService::new(
            Arc::new(db.to_string()),
            ws.clone(),
            crate::transcription::TranscriptionSettings {
                enabled: false,
                binary: String::new(),
                model: String::new(),
                language: "en".to_string(),
                vocabulary: String::new(),
                threads: 1,
                timeout: Duration::from_secs(1),
                models_dir: std::env::temp_dir(),
            },
            root.clone(),
        );
        RecordingSink {
            db_path: Arc::new(db.to_string()),
            ws_tx: ws.clone(),
            transcription,
            root,
        }
    }

    #[test]
    fn a_finalized_segment_is_written_to_the_hit_and_announced() {
        let db = unique_test_db_path("recordings.db");
        init_analytics_db(&db).expect("migrate");
        seeded_hit(&db, "h-1", "recording", None);
        let root = temp_root();
        let (ws, _) = broadcast::channel(16);
        let mut rx = ws.subscribe();
        let sink = sink(&db, &root, &ws);

        sink.persist(SegmentOutcome::Finalized {
            hit_uid: "h-1".to_string(),
            file_name: "h-1.wav".to_string(),
            duration_secs: 4.2,
            bytes: 134_400,
            status: SegmentStatus::Ready,
            confidence: LinkConfidence::Exact,
            error: None,
        });

        let hit = hit_store::get_hit(&db, "h-1").expect("row");
        assert_eq!(hit.recording_status, "ready");
        assert_eq!(hit.recording_file.as_deref(), Some("h-1.wav"));
        assert_eq!(hit.link_confidence.as_deref(), Some("exact"));
        assert_eq!(hit.recording_duration, Some(4.2));
        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(messages.iter().any(|m| m.contains("recording_ready")));
    }

    #[test]
    fn a_file_with_no_hit_row_is_deleted_rather_than_orphaned() {
        let db = unique_test_db_path("recordings-orphan.db");
        init_analytics_db(&db).expect("migrate");
        let root = temp_root();
        let orphan = root.join("no-row.wav");
        std::fs::write(&orphan, b"RIFF").unwrap();
        let (ws, _) = broadcast::channel(16);
        let sink = sink(&db, &root, &ws);

        sink.persist(SegmentOutcome::Finalized {
            hit_uid: "no-row".to_string(),
            file_name: "no-row.wav".to_string(),
            duration_secs: 1.0,
            bytes: 32_000,
            status: SegmentStatus::Ready,
            confidence: LinkConfidence::Exact,
            error: None,
        });

        assert!(!orphan.exists());
    }

    #[test]
    fn a_kerchunk_marks_the_hit_too_short_without_a_file() {
        let db = unique_test_db_path("recordings-short.db");
        init_analytics_db(&db).expect("migrate");
        seeded_hit(&db, "h-short", "recording", None);
        let root = temp_root();
        let (ws, _) = broadcast::channel(16);
        let sink = sink(&db, &root, &ws);

        sink.persist(SegmentOutcome::Discarded {
            hit_uid: "h-short".to_string(),
            reason: crate::audio::segment::DiscardReason::TooShort,
            duration_secs: 0.2,
        });

        let hit = hit_store::get_hit(&db, "h-short").unwrap();
        assert_eq!(hit.recording_status, "too_short");
        assert!(hit.recording_file.is_none());
    }

    #[test]
    fn a_capture_failure_keeps_the_hit_and_records_why() {
        let db = unique_test_db_path("recordings-failure.db");
        init_analytics_db(&db).expect("migrate");
        seeded_hit(&db, "h-fail", "recording", None);
        let root = temp_root();
        let (ws, _) = broadcast::channel(16);
        let sink = sink(&db, &root, &ws);

        sink.persist(SegmentOutcome::Failed {
            hit_uid: "h-fail".to_string(),
            error: "wav_create_failed: disk full".to_string(),
        });

        let hit = hit_store::get_hit(&db, "h-fail").unwrap();
        assert_eq!(hit.recording_status, "failed");
        assert!(hit
            .recording_error
            .as_deref()
            .is_some_and(|e| e.contains("disk full")));
    }

    fn service(db: &str, root: &PathBuf) -> Arc<RecordingService> {
        let (ws, _) = broadcast::channel(16);
        let root = Arc::new(Mutex::new(root.clone()));
        let transcription = TranscriptionService::new(
            Arc::new(db.to_string()),
            ws.clone(),
            crate::transcription::TranscriptionSettings {
                enabled: false,
                binary: String::new(),
                model: String::new(),
                language: "en".to_string(),
                vocabulary: String::new(),
                threads: 1,
                timeout: Duration::from_secs(1),
                models_dir: std::env::temp_dir(),
            },
            root.clone(),
        );
        RecordingService::new(
            Arc::new(db.to_string()),
            ws,
            transcription,
            RecordingSettings {
                retention_days: 0,
                ..Default::default()
            },
            root,
        )
    }

    #[test]
    fn housekeeping_recovers_interrupted_rows_and_finds_missing_files() {
        let db = unique_test_db_path("recordings-reconcile.db");
        init_analytics_db(&db).expect("migrate");
        let root = temp_root();
        seeded_hit(&db, "h-live", "recording", Some("h-live.wav"));
        seeded_hit(&db, "h-lost", "recording", Some("h-lost.wav"));
        seeded_hit(&db, "h-gone", "ready", Some("h-gone.wav"));
        std::fs::write(root.join("h-live.wav"), b"RIFF").unwrap();
        // A half-written file left by a crash.
        std::fs::write(root.join("crash.wav.part"), b"RIFF").unwrap();

        let service = service(&db, &root);
        let report = service.reconcile();

        assert_eq!(report.interrupted_rows, 2);
        assert_eq!(report.missing_rows, 1);
        assert_eq!(report.partial_files_removed, 1);
        let live = hit_store::get_hit(&db, "h-live").unwrap();
        assert_eq!(live.recording_status, "partial");
        assert_eq!(live.link_confidence.as_deref(), Some("uncertain"));
        assert_eq!(
            hit_store::get_hit(&db, "h-lost").unwrap().recording_status,
            "failed"
        );
        assert_eq!(
            hit_store::get_hit(&db, "h-gone").unwrap().recording_status,
            "missing"
        );
    }

    #[test]
    fn a_day_old_unreferenced_file_is_swept_and_a_fresh_one_is_left_alone() {
        let db = unique_test_db_path("recordings-sweep.db");
        init_analytics_db(&db).expect("migrate");
        let root = temp_root();
        let old = root.join("old-orphan.wav");
        let fresh = root.join("fresh-orphan.wav");
        std::fs::write(&old, b"RIFF").unwrap();
        std::fs::write(&fresh, b"RIFF").unwrap();
        let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        let handle = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
        handle
            .set_times(std::fs::FileTimes::new().set_modified(two_days_ago))
            .unwrap();

        let service = service(&db, &root);
        let report = service.reconcile();

        assert_eq!(report.orphan_files_removed, 1);
        assert!(!old.exists());
        assert!(
            fresh.exists(),
            "a file younger than a day may still get a row"
        );
    }

    /// The production worker, driven by injected audio instead of a device.
    ///
    /// This is the one test that walks the whole non-hardware path: samples and
    /// hit events in, a WAV on disk and the matching row's media columns out.
    /// `audio::segment` tests the timing rules and the sink tests the SQL; this
    /// pins that they are wired to each other and to the right uid.
    #[test]
    fn samples_and_hit_events_produce_a_file_and_a_linked_row() {
        let db = unique_test_db_path("recordings-worker.db");
        init_analytics_db(&db).expect("migrate");
        seeded_hit(&db, "h-live", "recording", None);
        let root = temp_root();
        let (ws, _) = broadcast::channel(32);
        let sink = sink(&db, &root, &ws);
        let (tx, rx) = std::sync::mpsc::channel();
        let bus = EventBus::new(tx, Arc::new(CaptureStatus::default()));
        let engine = SegmentEngine::new(
            SegmentConfig {
                sample_rate: crate::audio::RECORDING_SAMPLE_RATE,
                pre_roll: Duration::from_millis(100),
                post_roll: Duration::from_millis(100),
                min_transmission: Duration::from_millis(50),
            },
            root.clone(),
        );
        let worker_bus = bus.clone();
        let worker = thread::spawn(move || segment_worker(rx, engine, sink, worker_bus));

        // 100 ms of pre-roll, then the squelch opens.
        bus.send_samples(vec![0.2; 1_600]);
        bus.send(SegmentEvent::HitOpen {
            hit_uid: "h-live".to_string(),
        });
        bus.send_samples(vec![0.4; 16_000]);
        bus.send(SegmentEvent::HitClose);
        bus.send_samples(vec![0.0; 1_600]);
        // Give the worker a moment to drain the final post-roll, then stop it.
        thread::sleep(Duration::from_millis(200));
        bus.send(SegmentEvent::Stop);
        worker.join().expect("worker exits");

        let hit = hit_store::get_hit(&db, "h-live").expect("row");
        assert_eq!(hit.recording_status, "ready");
        assert_eq!(hit.recording_file.as_deref(), Some("h-live.wav"));
        let path = root.join("h-live.wav");
        assert!(path.is_file(), "the WAV must exist after a clean stop");
        let bytes = std::fs::metadata(&path).unwrap().len();
        assert!(bytes > 32_000, "about 1.1 s of 16 kHz audio, got {bytes}");
        // The file is a real WAV, not an empty placeholder.
        let reader = hound::WavReader::open(&path).expect("valid WAV");
        assert_eq!(
            reader.spec().sample_rate,
            crate::audio::RECORDING_SAMPLE_RATE
        );
        assert_eq!(reader.spec().channels, 1);
    }

    #[test]
    fn two_transmissions_produce_two_files_without_overwriting() {
        let db = unique_test_db_path("recordings-worker-two.db");
        init_analytics_db(&db).expect("migrate");
        seeded_hit(&db, "h-a", "recording", None);
        seeded_hit(&db, "h-b", "recording", None);
        let root = temp_root();
        let (ws, _) = broadcast::channel(32);
        let sink = sink(&db, &root, &ws);
        let (tx, rx) = std::sync::mpsc::channel();
        let bus = EventBus::new(tx, Arc::new(CaptureStatus::default()));
        let engine = SegmentEngine::new(
            SegmentConfig {
                sample_rate: crate::audio::RECORDING_SAMPLE_RATE,
                pre_roll: Duration::ZERO,
                post_roll: Duration::ZERO,
                min_transmission: Duration::from_millis(50),
            },
            root.clone(),
        );
        let worker_bus = bus.clone();
        let worker = thread::spawn(move || segment_worker(rx, engine, sink, worker_bus));

        for uid in ["h-a", "h-b"] {
            bus.send(SegmentEvent::HitOpen {
                hit_uid: uid.to_string(),
            });
            bus.send_samples(vec![0.3; 8_000]);
            bus.send(SegmentEvent::HitClose);
        }
        thread::sleep(Duration::from_millis(150));
        bus.send(SegmentEvent::Stop);
        worker.join().expect("worker exits");

        for uid in ["h-a", "h-b"] {
            let hit = hit_store::get_hit(&db, uid).expect("row");
            assert_eq!(hit.recording_status, "ready", "{uid}");
            assert!(root.join(format!("{uid}.wav")).is_file(), "{uid}");
        }
    }

    #[test]
    fn the_initial_status_reflects_an_active_segment() {
        let db = unique_test_db_path("recordings-status.db");
        init_analytics_db(&db).expect("migrate");
        let root = temp_root();
        let service = service(&db, &root);
        assert_eq!(service.initial_status("h-1"), "none");
        // Recording is disabled in this service, so `hit_open` is a no-op and
        // the status stays honest rather than claiming a file is coming.
        service.hit_open("h-1");
        assert_eq!(service.initial_status("h-1"), "none");
    }

    #[test]
    fn an_absolute_or_traversing_reference_is_refused() {
        let root = Path::new("/tmp/recordings");
        assert!(hit_store::safe_join(root, "/etc/passwd").is_none());
        assert!(hit_store::safe_join(root, "../escape.wav").is_none());
    }
}
