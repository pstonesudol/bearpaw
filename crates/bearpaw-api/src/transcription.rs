//! Local, offline speech-to-text for hit recordings.
//!
//! The engine is an external `whisper-cli` process (from `whisper.cpp`) against
//! a local GGML model. Two deliberate consequences:
//!
//! - A missing engine or model is a **named, recoverable job failure**, never a
//!   build-time or startup problem. Recording keeps working without it.
//! - Nothing is uploaded. There is no network path in this module at all.
//!
//! Jobs run one at a time on a dedicated thread and are persisted before and
//! after, so a quit mid-transcription leaves a `pending` row that the next
//! launch picks up.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::broadcast;

use crate::api::hit_store;

pub const ENGINE_NAME: &str = "whisper-cli";

/// A starting vocabulary for the radio traffic Bearpaw is most often used for.
/// It is a prompt, not a dictionary: whisper biases toward these spellings.
pub const DEFAULT_VOCABULARY: &str = "SEPTA, Norfolk Southern, Amtrak, NORAC, \
Harrisburg Line, Keystone Corridor, Zoo Interlocking, Phoenix, Paoli, Thorndale, \
Norristown, Powelton Yard, clear, approach, restricted speed, dispatcher, \
maintenance of way";

/// whisper's initial prompt window is a few hundred tokens, not a glossary.
/// Sending the whole of a long list wastes context and can push the useful
/// terms out, so the tail is dropped with an ellipsis the model ignores.
const MAX_PROMPT_CHARS: usize = 400;

#[derive(Clone, Debug)]
pub struct TranscriptionSettings {
    pub enabled: bool,
    /// Explicit binary path or bare command name from preferences.
    pub binary: String,
    /// Explicit model path, or a file name inside `models_dir`.
    pub model: String,
    pub language: String,
    pub vocabulary: String,
    pub threads: u16,
    pub timeout: Duration,
    pub models_dir: PathBuf,
}

impl TranscriptionSettings {
    pub fn from_preferences(prefs: &Map<String, Value>, data_dir: &Path) -> Self {
        let string = |key: &str, fallback: &str| {
            prefs
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        let enabled = prefs
            .get("transcription_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let threads = prefs
            .get("transcription_threads")
            .and_then(Value::as_u64)
            .map(|v| v.clamp(1, 32) as u16)
            .unwrap_or(4);
        Self {
            enabled,
            binary: prefs
                .get("transcription_binary")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            model: prefs
                .get("transcription_model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            language: string("transcription_language", "en"),
            vocabulary: string("transcription_vocabulary", DEFAULT_VOCABULARY),
            threads,
            timeout: Duration::from_secs(
                prefs
                    .get("transcription_timeout_secs")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(30, 1800))
                    .unwrap_or(300),
            ),
            models_dir: data_dir.join("models"),
        }
    }

    pub fn resolved_binary(&self) -> Option<PathBuf> {
        resolve_binary(&self.binary)
    }

    pub fn resolved_model(&self) -> Option<PathBuf> {
        resolve_model(&self.model, &self.models_dir)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
}

/// Discover GGML models the user has put in the app-data `models/` directory.
/// A model anywhere else still works — the selector accepts a typed path — but
/// discovery is deliberately limited to one directory rather than scanning the
/// disk.
pub fn list_models(models_dir: &Path) -> Vec<ModelInfo> {
    let Ok(entries) = std::fs::read_dir(models_dir) else {
        return Vec::new();
    };
    let mut models: Vec<ModelInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let is_bin = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("bin"));
            if !is_bin || !path.is_file() {
                return None;
            }
            Some(ModelInfo {
                name: path.file_name()?.to_string_lossy().into_owned(),
                path: path.to_string_lossy().into_owned(),
                size_bytes: entry.metadata().ok().map(|m| m.len()).unwrap_or(0),
            })
        })
        .collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    models
}

fn resolve_binary(explicit: &str) -> Option<PathBuf> {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        // A bare name is still allowed to resolve through PATH: a user who
        // types "whisper-cli" should not be told it is missing on a machine
        // where `which whisper-cli` succeeds.
        if !explicit.contains('/') {
            return find_on_path(explicit);
        }
        return None;
    }
    find_on_path("whisper-cli")
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // A GUI app launched from Finder does not inherit a shell PATH. These are
    // the two locations Homebrew installs to on Apple Silicon and Intel.
    for fallback in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let candidate = Path::new(fallback).join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn resolve_model(explicit: &str, models_dir: &Path) -> Option<PathBuf> {
    let explicit = explicit.trim();
    if !explicit.is_empty() {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
        if !explicit.contains('/') {
            let inside = models_dir.join(explicit);
            if inside.is_file() {
                return Some(inside);
            }
        }
        return None;
    }
    // Auto-select the first discovered model so a single-model install needs no
    // configuration at all.
    list_models(models_dir)
        .into_iter()
        .next()
        .map(|m| PathBuf::from(m.path))
}

#[derive(Clone, Debug)]
pub struct TranscriptionRequest {
    pub audio: PathBuf,
    pub model: PathBuf,
    pub language: String,
    pub vocabulary: String,
    pub threads: u16,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptionError {
    BinaryMissing(String),
    ModelMissing(String),
    AudioMissing(PathBuf),
    SpawnFailed(String),
    Timeout(Duration),
    EmptyTranscript,
    EngineFailed { code: Option<i32>, detail: String },
}

impl TranscriptionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BinaryMissing(_) => "transcription_binary_missing",
            Self::ModelMissing(_) => "transcription_model_missing",
            Self::AudioMissing(_) => "audio_missing",
            Self::SpawnFailed(_) => "transcription_spawn_failed",
            Self::Timeout(_) => "transcription_timeout",
            Self::EmptyTranscript => "transcription_empty",
            Self::EngineFailed { .. } => "transcription_engine_failed",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::BinaryMissing(path) => format!(
                "whisper-cli was not found ({path}). Install it with `brew install whisper-cpp`, \
                 or set the transcription binary in Device → Audio & Transcription."
            ),
            Self::ModelMissing(path) => format!(
                "No whisper model found ({path}). Put a GGML `.bin` model in the Bearpaw models \
                 directory, or select one in Device → Audio & Transcription."
            ),
            Self::AudioMissing(path) => {
                format!("The recording file is missing: {}", path.display())
            }
            Self::SpawnFailed(detail) => format!("whisper-cli could not be started: {detail}"),
            Self::Timeout(limit) => {
                format!(
                    "whisper-cli did not finish within {} seconds",
                    limit.as_secs()
                )
            }
            Self::EmptyTranscript => {
                "whisper-cli produced no text. The transmission may be noise or too quiet."
                    .to_string()
            }
            Self::EngineFailed { code, detail } => match code {
                Some(code) => format!("whisper-cli exited with status {code}: {detail}"),
                None => format!("whisper-cli failed: {detail}"),
            },
        }
    }
}

pub trait TranscriptionEngine: Send + Sync {
    fn name(&self) -> &'static str;
    fn transcribe(&self, request: &TranscriptionRequest) -> Result<String, TranscriptionError>;
}

/// Runs `whisper-cli` and reads the text file it writes.
pub struct WhisperCliEngine {
    binary: PathBuf,
}

impl WhisperCliEngine {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }
}

impl TranscriptionEngine for WhisperCliEngine {
    fn name(&self) -> &'static str {
        ENGINE_NAME
    }

    fn transcribe(&self, request: &TranscriptionRequest) -> Result<String, TranscriptionError> {
        if !request.audio.is_file() {
            return Err(TranscriptionError::AudioMissing(request.audio.clone()));
        }
        if !self.binary.is_file() {
            return Err(TranscriptionError::BinaryMissing(
                self.binary.to_string_lossy().into_owned(),
            ));
        }
        if !request.model.is_file() {
            return Err(TranscriptionError::ModelMissing(
                request.model.to_string_lossy().into_owned(),
            ));
        }

        let stem = request
            .audio
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "hit".to_string());
        let work_base = request.audio.with_file_name(format!(".{stem}.whisper"));
        // whisper appends `.txt` to the `--output-file` base; it does not
        // replace the base's extension. `with_extension("txt")` therefore
        // looked for `.stem.txt` while whisper wrote `.stem.whisper.txt`, and
        // every job reported an empty transcript.
        let text_path = PathBuf::from(format!("{}.txt", work_base.display()));
        let log_path = PathBuf::from(format!("{}.log", work_base.display()));
        let _ = std::fs::remove_file(&text_path);
        let _ = std::fs::remove_file(&log_path);

        let log_file = std::fs::File::create(&log_path)
            .map_err(|e| TranscriptionError::SpawnFailed(format!("log file: {e}")))?;

        let prompt = truncate_prompt(&request.vocabulary);
        let mut command = Command::new(&self.binary);
        command
            .arg("--model")
            .arg(&request.model)
            .arg("--file")
            .arg(&request.audio)
            .arg("--language")
            .arg(&request.language)
            .arg("--threads")
            .arg(request.threads.max(1).to_string())
            .arg("--no-timestamps")
            .arg("--output-txt")
            .arg("--output-file")
            .arg(&work_base)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Redirected to a file rather than a pipe: whisper writes progress
            // to stderr continuously, and an undrained pipe deadlocks the child
            // long before the timeout would fire.
            .stderr(Stdio::from(log_file));
        if !prompt.is_empty() {
            command.arg("--prompt").arg(&prompt);
        }

        let mut child = command.spawn().map_err(|e| {
            TranscriptionError::SpawnFailed(format!("{}: {e}", self.binary.display()))
        })?;

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started.elapsed() >= request.timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        cleanup(&text_path, &log_path);
                        return Err(TranscriptionError::Timeout(request.timeout));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    let _ = child.kill();
                    cleanup(&text_path, &log_path);
                    return Err(TranscriptionError::SpawnFailed(format!("wait: {e}")));
                }
            }
        };

        if !status.success() {
            let detail = read_log_tail(&log_path);
            cleanup(&text_path, &log_path);
            return Err(TranscriptionError::EngineFailed {
                code: status.code(),
                detail,
            });
        }

        let text = std::fs::read_to_string(&text_path)
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        cleanup(&text_path, &log_path);
        // whisper marks non-speech with a single bracketed or parenthesised
        // tag (`[BLANK_AUDIO]`, `[APPLAUSE]`, `(train horn)`) rather than an
        // empty file. Storing that as a transcript presents noise as a result.
        let is_marker = (text.starts_with('[') && text.ends_with(']'))
            || (text.starts_with('(') && text.ends_with(')'));
        if text.is_empty() || is_marker {
            return Err(TranscriptionError::EmptyTranscript);
        }
        Ok(text)
    }
}

fn cleanup(text_path: &Path, log_path: &Path) {
    let _ = std::fs::remove_file(text_path);
    let _ = std::fs::remove_file(log_path);
}

fn truncate_prompt(vocabulary: &str) -> String {
    let trimmed = vocabulary.trim();
    if trimmed.chars().count() <= MAX_PROMPT_CHARS {
        return trimmed.to_string();
    }
    let truncated: String = trimmed.chars().take(MAX_PROMPT_CHARS).collect();
    // Cut at a word boundary so the model is not handed half a term.
    match truncated.rfind([' ', ',']) {
        Some(idx) => truncated[..idx].to_string(),
        None => truncated,
    }
}

fn read_log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "no output".to_string();
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines
        .iter()
        .rev()
        .take(3)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join(" | ");
    if tail.is_empty() {
        "no output".to_string()
    } else if tail.chars().count() > 400 {
        tail.chars().take(400).collect()
    } else {
        tail
    }
}

enum Job {
    Transcribe { hit_uid: String, force: bool },
    Stop,
}

/// Owns the job thread and the settings snapshot.
pub struct TranscriptionService {
    /// Created by `start()`. Holding it behind a mutex means a caller that
    /// enqueues before startup is not silently dropped: `enqueue` starts the
    /// worker first, then sends.
    sender: Mutex<Option<Sender<Job>>>,
    db_path: Arc<String>,
    ws_tx: broadcast::Sender<String>,
    settings: Arc<Mutex<TranscriptionSettings>>,
    root: Arc<Mutex<PathBuf>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    started: AtomicBool,
    /// Held across `start`/`stop` so the sender and the worker handle cannot be
    /// observed half-published.
    start_lock: Mutex<()>,
}

impl TranscriptionService {
    pub fn new(
        db_path: Arc<String>,
        ws_tx: broadcast::Sender<String>,
        settings: TranscriptionSettings,
        root: Arc<Mutex<PathBuf>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sender: Mutex::new(None),
            db_path,
            ws_tx,
            settings: Arc::new(Mutex::new(settings)),
            root,
            worker: Mutex::new(None),
            started: AtomicBool::new(false),
            start_lock: Mutex::new(()),
        })
    }

    pub fn settings(&self) -> TranscriptionSettings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_else(|_| {
            TranscriptionSettings::from_preferences(&Map::new(), Path::new("."))
        })
    }

    /// Replace the settings snapshot the worker reads before each job.
    ///
    /// Re-enabling after a disabled period re-queues anything still `pending`:
    /// jobs skipped while transcription was off are harmless to repeat (a
    /// completed row is skipped unless forced) and leaving them queued forever
    /// would show "Queued" with no worker to run them.
    pub fn update_settings(&self, settings: TranscriptionSettings) {
        let was_enabled = self.settings().enabled;
        if let Ok(mut slot) = self.settings.lock() {
            *slot = settings.clone();
        }
        // Re-enabling after a disabled period needs a worker and a requeue.
        // `start` performs the requeue itself, so doing it here as well would
        // enqueue the backlog twice.
        if settings.enabled && !was_enabled {
            self.start();
        }
    }

    /// Start the worker once.
    ///
    /// Serialised: a caller that loses the race must wait until the winner has
    /// published the sender, or its job is silently dropped. `started` is set
    /// last, after the sender exists.
    pub fn start(&self) {
        let _start = self.start_lock.lock();
        if self.started.load(Ordering::SeqCst) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let db_path = self.db_path.clone();
        let ws_tx = self.ws_tx.clone();
        let settings = self.settings.clone();
        let root = self.root.clone();
        let handle = thread::Builder::new()
            .name("bearpaw-transcription".to_string())
            .spawn(move || worker_loop(rx, db_path, ws_tx, settings, root))
            .ok();
        if let Ok(mut worker) = self.worker.lock() {
            *worker = handle;
        }
        if let Ok(mut slot) = self.sender.lock() {
            *slot = Some(tx.clone());
        }
        self.started.store(true, Ordering::SeqCst);
        // Any job interrupted by the last shutdown is queued again. Send on the
        // local sender rather than calling `enqueue`: that would re-enter
        // `start` and deadlock on `start_lock`.
        if self.settings().enabled {
            for uid in hit_store::pending_transcription_uids(&self.db_path) {
                let _ = tx.send(Job::Transcribe {
                    hit_uid: uid,
                    force: false,
                });
            }
        }
    }

    /// Ask the worker to stop.
    ///
    /// Deliberately does NOT join. A `whisper-cli` job can run for minutes
    /// (the timeout is 300 s by default), and joining would make quitting the
    /// app hang until the model finished. The worker sees `Stop` after the
    /// current job; if the process exits first, the row is still `processing`
    /// and the next launch requeues it. The handle is dropped (detached), so
    /// nothing waits on it.
    pub fn stop(&self) {
        let _start = self.start_lock.lock();
        if !self.started.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Ok(slot) = self.sender.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(Job::Stop);
            }
        }
        if let Ok(mut worker) = self.worker.lock() {
            let _ = worker.take();
        }
    }

    pub fn enqueue(&self, hit_uid: &str) {
        if !self.settings().enabled {
            return;
        }
        self.start();
        if let Ok(slot) = self.sender.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(Job::Transcribe {
                    hit_uid: hit_uid.to_string(),
                    force: false,
                });
            }
        }
    }

    /// Manual retry. `force` also retranscribes a `completed` row.
    pub fn retry(&self, hit_uid: &str, force: bool) {
        self.start();
        if let Ok(slot) = self.sender.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(Job::Transcribe {
                    hit_uid: hit_uid.to_string(),
                    force,
                });
            }
        }
    }
}

fn worker_loop(
    rx: Receiver<Job>,
    db_path: Arc<String>,
    ws_tx: broadcast::Sender<String>,
    settings: Arc<Mutex<TranscriptionSettings>>,
    root: Arc<Mutex<PathBuf>>,
) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Stop => break,
            Job::Transcribe { hit_uid, force } => {
                let snapshot = settings.lock().map(|s| s.clone()).unwrap_or_else(|_| {
                    TranscriptionSettings::from_preferences(&Map::new(), Path::new("."))
                });
                run_job(&db_path, &ws_tx, &root, &snapshot, &hit_uid, force, None);
            }
        }
    }
}

fn run_job(
    db_path: &str,
    ws_tx: &broadcast::Sender<String>,
    root: &Arc<Mutex<PathBuf>>,
    settings: &TranscriptionSettings,
    hit_uid: &str,
    force: bool,
    // Tests inject an engine here. Production passes `None` and the job
    // resolves `whisper-cli` for itself, so the resolution path is the one
    // exercised in the field.
    engine_override: Option<&dyn TranscriptionEngine>,
) {
    let Some(hit) = hit_store::get_hit(db_path, hit_uid) else {
        return;
    };
    if !force && hit.transcript_status == "completed" {
        return;
    }
    if !settings.enabled {
        return;
    }
    if !matches!(hit.recording_status.as_str(), "ready" | "partial") {
        fail(
            db_path,
            ws_tx,
            hit_uid,
            &TranscriptionError::AudioMissing(PathBuf::from("no recording for this hit")),
            false,
        );
        return;
    }
    let Some(file_name) = hit.recording_file.as_deref() else {
        fail(
            db_path,
            ws_tx,
            hit_uid,
            &TranscriptionError::AudioMissing(PathBuf::from("no recording for this hit")),
            false,
        );
        return;
    };
    let root_path = root.lock().map(|r| r.clone()).unwrap_or_default();
    let Some(audio) = hit_store::safe_join(&root_path, file_name) else {
        fail(
            db_path,
            ws_tx,
            hit_uid,
            &TranscriptionError::AudioMissing(PathBuf::from(file_name)),
            false,
        );
        return;
    };

    hit_store::set_transcript_status(db_path, hit_uid, "processing", None, false);
    broadcast_status(ws_tx, hit_uid, "processing", None, None, None);

    let (engine_name, result) = match engine_override {
        Some(engine) => {
            let request = TranscriptionRequest {
                audio,
                model: PathBuf::new(),
                language: settings.language.clone(),
                vocabulary: settings.vocabulary.clone(),
                threads: settings.threads,
                timeout: settings.timeout,
            };
            (engine.name(), engine.transcribe(&request))
        }
        None => {
            let binary = match settings.resolved_binary() {
                Some(path) => path,
                None => {
                    let error = TranscriptionError::BinaryMissing(if settings.binary.is_empty() {
                        "whisper-cli".to_string()
                    } else {
                        settings.binary.clone()
                    });
                    fail(db_path, ws_tx, hit_uid, &error, true);
                    return;
                }
            };
            let model = match settings.resolved_model() {
                Some(path) => path,
                None => {
                    let error = TranscriptionError::ModelMissing(settings.model.clone());
                    fail(db_path, ws_tx, hit_uid, &error, true);
                    return;
                }
            };
            let engine = WhisperCliEngine::new(binary);
            let request = TranscriptionRequest {
                audio,
                model,
                language: settings.language.clone(),
                vocabulary: settings.vocabulary.clone(),
                threads: settings.threads,
                timeout: settings.timeout,
            };
            (engine.name(), engine.transcribe(&request))
        }
    };

    match result {
        Ok(text) => {
            let stored = hit_store::set_transcript_completed(db_path, hit_uid, &text, engine_name);
            if stored {
                broadcast_status(
                    ws_tx,
                    hit_uid,
                    "completed",
                    Some(&text),
                    Some(engine_name),
                    None,
                );
            }
        }
        Err(error) => fail(db_path, ws_tx, hit_uid, &error, true),
    }
}

fn fail(
    db_path: &str,
    ws_tx: &broadcast::Sender<String>,
    hit_uid: &str,
    error: &TranscriptionError,
    count_attempt: bool,
) {
    let message = format!("{}: {}", error.code(), error.message());
    hit_store::set_transcript_status(db_path, hit_uid, "failed", Some(&message), count_attempt);
    broadcast_status(ws_tx, hit_uid, "failed", None, None, Some(&message));
    tracing::warn!(hit_uid, error = %message, "transcription failed");
}

fn broadcast_status(
    ws_tx: &broadcast::Sender<String>,
    hit_uid: &str,
    status: &str,
    transcript: Option<&str>,
    engine: Option<&str>,
    error: Option<&str>,
) {
    let _ = ws_tx.send(
        json!({
            "type": "event",
            "timestamp": crate::api::epoch_now(),
            "event": "transcript_ready",
            "data": {
                "hit_uid": hit_uid,
                "transcript_status": status,
                "transcript": transcript,
                "transcript_engine": engine,
                "transcript_error": error,
            }
        })
        .to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeEngine {
        result: Result<String, TranscriptionError>,
        calls: Arc<Mutex<usize>>,
    }

    impl TranscriptionEngine for FakeEngine {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn transcribe(
            &self,
            _request: &TranscriptionRequest,
        ) -> Result<String, TranscriptionError> {
            *self.calls.lock().unwrap() += 1;
            self.result.clone()
        }
    }

    #[test]
    fn a_long_vocabulary_is_cut_at_a_word_boundary() {
        let long = "SEPTA ".repeat(200);
        let prompt = truncate_prompt(&long);
        assert!(prompt.chars().count() <= MAX_PROMPT_CHARS);
        assert!(prompt.ends_with("SEPTA"), "{prompt}");
        assert!(!prompt.ends_with("SEPT"));
    }

    #[test]
    fn a_short_vocabulary_is_passed_through() {
        assert_eq!(truncate_prompt("  SEPTA, Amtrak  "), "SEPTA, Amtrak");
    }

    #[test]
    fn missing_binaries_and_models_are_named_failures() {
        let engine = WhisperCliEngine::new(PathBuf::from("/nonexistent/whisper-cli"));
        let result = engine.transcribe(&TranscriptionRequest {
            audio: PathBuf::from("/nonexistent.wav"),
            model: PathBuf::from("/nonexistent.bin"),
            language: "en".into(),
            vocabulary: String::new(),
            threads: 1,
            timeout: Duration::from_secs(1),
        });
        assert!(matches!(result, Err(TranscriptionError::AudioMissing(_))));
    }

    /// REGRESSION GUARD: whisper appends `.txt` to the `--output-file` base.
    ///
    /// The first implementation used `with_extension("txt")` on a base ending
    /// in `.whisper`, so it looked for `.hit.txt` while whisper wrote
    /// `.hit.whisper.txt`. Every real transcription failed as "empty" while
    /// every test passed, because the tests used a fake engine and never
    /// exercised this path. Found on hardware, 2026-10-26.
    #[cfg(unix)]
    fn script_engine(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-whisper");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[cfg(unix)]
    fn engine_request(dir: &Path) -> TranscriptionRequest {
        let audio = dir.join("hit.wav");
        let model = dir.join("model.bin");
        std::fs::write(&audio, b"RIFF").unwrap();
        std::fs::write(&model, b"model").unwrap();
        TranscriptionRequest {
            audio,
            model,
            language: "en".into(),
            vocabulary: "SEPTA, Norfolk Southern".into(),
            threads: 1,
            timeout: Duration::from_secs(5),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_engine_reads_the_text_file_whisper_actually_writes() {
        let dir = std::env::temp_dir().join(format!(
            "bearpaw-whisper-cli-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = script_engine(
            &dir,
            "while [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--output-file\" ]; then out=\"$2\"; fi\n  shift\ndone\nprintf 'Norfolk Southern 24 clear' > \"$out.txt\"",
        );
        let request = engine_request(&dir);
        let engine = WhisperCliEngine::new(script);
        let text = engine.transcribe(&request).expect("the transcript");
        assert_eq!(text, "Norfolk Southern 24 clear");
        assert!(
            !dir.join(".hit.whisper.txt").exists(),
            "the output file must be cleaned up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_bracketed_non_speech_marker_is_an_empty_transcript_not_text() {
        for marker in ["[BLANK_AUDIO]", "[APPLAUSE]", "(train horn)"] {
            let dir = std::env::temp_dir().join(format!(
                "bearpaw-whisper-cli-{}-{}",
                std::process::id(),
                crate::api::uuid_simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let script = script_engine(
                &dir,
                &format!(
                    "while [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--output-file\" ]; then out=\"$2\"; fi\n  shift\ndone\nprintf '%s' '{marker}' > \"$out.txt\""
                ),
            );
            let request = engine_request(&dir);
            let engine = WhisperCliEngine::new(script);
            assert!(
                matches!(
                    engine.transcribe(&request),
                    Err(TranscriptionError::EmptyTranscript)
                ),
                "{marker} must not become a transcript"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn the_engine_failure_codes_are_stable() {
        assert_eq!(
            TranscriptionError::EmptyTranscript.code(),
            "transcription_empty"
        );
        assert_eq!(
            TranscriptionError::BinaryMissing("x".into()).code(),
            "transcription_binary_missing"
        );
        assert!(TranscriptionError::ModelMissing("x".into())
            .message()
            .contains("models"));
    }

    #[test]
    fn models_are_discovered_from_the_app_data_directory() {
        let dir = std::env::temp_dir().join(format!(
            "bearpaw-models-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ggml-base.en.bin"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        let models = list_models(&dir);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "ggml-base.en.bin");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_model_name_resolves_inside_the_models_directory() {
        let dir = std::env::temp_dir().join(format!(
            "bearpaw-models-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ggml-small.en.bin"), b"x").unwrap();
        let resolved = resolve_model("ggml-small.en.bin", &dir);
        assert_eq!(resolved, Some(dir.join("ggml-small.en.bin")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::api::{insert_analytics_hit, unique_test_db_path, ActivityHit};
    use crate::state::ScannerMode;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    struct ScriptedEngine {
        results: Mutex<VecDeque<Result<String, TranscriptionError>>>,
        calls: AtomicUsize,
    }

    impl ScriptedEngine {
        fn new(results: Vec<Result<String, TranscriptionError>>) -> Self {
            Self {
                results: Mutex::new(results.into()),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl TranscriptionEngine for ScriptedEngine {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn transcribe(&self, request: &TranscriptionRequest) -> Result<String, TranscriptionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !request.audio.is_file() {
                return Err(TranscriptionError::AudioMissing(request.audio.clone()));
            }
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("clear".to_string()))
        }
    }

    fn seeded() -> (
        String,
        Arc<Mutex<PathBuf>>,
        PathBuf,
        broadcast::Sender<String>,
    ) {
        let db = unique_test_db_path("transcribe.db");
        crate::api::init_analytics_db(&db).expect("analytics migration");
        let root = std::env::temp_dir().join(format!(
            "bearpaw-transcribe-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        insert_analytics_hit(
            &db,
            &ActivityHit {
                id: "1".to_string(),
                timestamp: crate::api::epoch_now(),
                frequency: 161.1,
                channel: Some(12),
                alpha_tag: Some("AMTRAK".to_string()),
                rssi: 40,
                duration: 3.0,
                modulation: "NFM".to_string(),
                mode: ScannerMode::Scan,
                bank: Some(2),
                session_id: "s".to_string(),
                ended_at: crate::api::epoch_now() + 3.0,
                scanner_id: Some("BC125AT".to_string()),
                hit_uid: Some("h-tx".to_string()),
                ..Default::default()
            },
        );
        hit_store::set_recording_outcome(
            &db,
            "h-tx",
            Some("h-tx.wav"),
            "ready",
            Some("exact"),
            Some(3.0),
            Some(96_000),
            None,
        );
        std::fs::write(root.join("h-tx.wav"), b"RIFF....").unwrap();
        let (ws, _) = broadcast::channel(32);
        (db, Arc::new(Mutex::new(root.clone())), root, ws)
    }

    fn settings() -> TranscriptionSettings {
        TranscriptionSettings {
            enabled: true,
            binary: String::new(),
            model: String::new(),
            language: "en".to_string(),
            vocabulary: "SEPTA, Amtrak".to_string(),
            threads: 1,
            timeout: Duration::from_secs(5),
            models_dir: std::env::temp_dir(),
        }
    }

    fn run(
        db: &str,
        root: &Arc<Mutex<PathBuf>>,
        ws: &broadcast::Sender<String>,
        engine: &dyn TranscriptionEngine,
        force: bool,
    ) {
        run_job(db, ws, root, &settings(), "h-tx", force, Some(engine));
    }

    #[test]
    fn a_successful_job_persists_text_and_announces_it() {
        let (db, root, _dir, ws) = seeded();
        let mut rx = ws.subscribe();
        let engine = ScriptedEngine::new(vec![Ok("Amtrak 215 clear at Zoo".to_string())]);
        run(&db, &root, &ws, &engine, false);

        let hit = hit_store::get_hit(&db, "h-tx").expect("row");
        assert_eq!(hit.transcript_status, "completed");
        assert_eq!(hit.transcript.as_deref(), Some("Amtrak 215 clear at Zoo"));
        assert_eq!(hit.transcript_engine.as_deref(), Some("fake"));
        assert!(hit.transcript_error.is_none());

        let messages: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(messages
            .iter()
            .any(|m| m.contains("\"transcript_status\":\"completed\"")));
        assert_eq!(engine.calls(), 1);
    }

    #[test]
    fn a_failed_job_is_retryable_and_the_retry_succeeds() {
        let (db, root, _dir, ws) = seeded();
        let engine = ScriptedEngine::new(vec![
            Err(TranscriptionError::EmptyTranscript),
            Ok("SEPTA 421 approach".to_string()),
        ]);
        run(&db, &root, &ws, &engine, false);
        let failed = hit_store::get_hit(&db, "h-tx").unwrap();
        assert_eq!(failed.transcript_status, "failed");
        assert!(failed
            .transcript_error
            .as_deref()
            .is_some_and(|e| e.contains("transcription_empty")));
        assert_eq!(failed.transcript_attempts, 1);
        // The audio survives a transcription failure: retrying needs it.
        assert_eq!(failed.recording_status, "ready");

        run(&db, &root, &ws, &engine, true);
        let completed = hit_store::get_hit(&db, "h-tx").unwrap();
        assert_eq!(completed.transcript_status, "completed");
        assert_eq!(completed.transcript.as_deref(), Some("SEPTA 421 approach"));
        assert_eq!(engine.calls(), 2);
    }

    #[test]
    fn a_completed_transcript_is_not_retranscribed_unless_forced() {
        let (db, root, _dir, ws) = seeded();
        let engine = ScriptedEngine::new(vec![Ok("first".into()), Ok("second".into())]);
        run(&db, &root, &ws, &engine, false);
        run(&db, &root, &ws, &engine, false);
        assert_eq!(engine.calls(), 1);
        run(&db, &root, &ws, &engine, true);
        assert_eq!(engine.calls(), 2);
        assert_eq!(
            hit_store::get_hit(&db, "h-tx")
                .unwrap()
                .transcript
                .as_deref(),
            Some("second")
        );
    }

    #[test]
    fn a_hit_with_no_audio_fails_with_an_actionable_code() {
        let (db, root, _dir, ws) = seeded();
        std::fs::remove_file(root.lock().unwrap().join("h-tx.wav")).unwrap();
        let engine = ScriptedEngine::new(vec![Ok("never".into())]);
        run(&db, &root, &ws, &engine, false);
        let hit = hit_store::get_hit(&db, "h-tx").unwrap();
        assert_eq!(hit.transcript_status, "failed");
        assert!(hit
            .transcript_error
            .as_deref()
            .is_some_and(|e| e.contains("audio_missing")));
    }

    #[test]
    fn disabling_transcription_leaves_the_hit_alone() {
        let (db, root, _dir, ws) = seeded();
        let engine = ScriptedEngine::new(vec![Ok("never".into())]);
        let mut off = settings();
        off.enabled = false;
        run_job(&db, &ws, &root, &off, "h-tx", true, Some(&engine));
        let hit = hit_store::get_hit(&db, "h-tx").unwrap();
        assert_eq!(hit.transcript_status, "none");
        assert_eq!(engine.calls(), 0);
    }

    #[test]
    fn the_fake_engine_is_what_the_job_runner_uses() {
        // The job runner is exercised through the service in the API tests;
        // this pins the trait shape so a real engine cannot silently change it.
        let calls = Arc::new(Mutex::new(0));
        let engine = FakeEngine {
            result: Ok("clear".into()),
            calls: calls.clone(),
        };
        let text = engine
            .transcribe(&TranscriptionRequest {
                audio: PathBuf::new(),
                model: PathBuf::new(),
                language: "en".into(),
                vocabulary: String::new(),
                threads: 1,
                timeout: Duration::from_secs(1),
            })
            .unwrap();
        assert_eq!(text, "clear");
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}
