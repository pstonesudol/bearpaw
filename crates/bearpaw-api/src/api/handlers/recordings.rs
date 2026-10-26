//! REST surface for recordings, transcripts, the audio device list, and
//! searchable hit history.
//!
//! Audio files are served from here rather than exposed by a static route: the
//! database holds a relative name, the recordings root is a preference, and the
//! join between them must refuse `..`. One handler owns that decision.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::{hit_store, ApiError, AppState};

#[derive(Deserialize)]
pub(crate) struct TestClipRequest {
    /// Seconds to record. Clamped by the recorder to 0.25–30.
    seconds: Option<f64>,
}

pub(crate) async fn get_audio_devices(State(state): State<AppState>) -> Json<Value> {
    // Device enumeration talks to CoreAudio and can block while a USB device
    // settles, so it must not run on a runtime worker.
    let recordings = state.recordings.clone();
    let devices = tokio::task::spawn_blocking(move || recordings.list_devices())
        .await
        .unwrap_or_default();
    let selected = state.recordings.settings().device;
    let transcription = state.transcription.settings();
    let models = crate::transcription::list_models(&transcription.models_dir);
    let resolved_binary = transcription
        .resolved_binary()
        .map(|p| p.to_string_lossy().into_owned());
    let resolved_model = transcription
        .resolved_model()
        .map(|p| p.to_string_lossy().into_owned());
    Json(json!({
        "devices": devices,
        "selected": selected,
        "status": state.recordings.status(),
        "models": models,
        "models_dir": transcription.models_dir.to_string_lossy(),
        "transcription": {
            "enabled": transcription.enabled,
            "language": transcription.language,
            "resolved_binary": resolved_binary,
            "resolved_model": resolved_model,
        },
    }))
}

pub(crate) async fn get_audio_status(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.recordings.status()))
}

pub(crate) async fn post_audio_test(
    State(state): State<AppState>,
    Json(body): Json<TestClipRequest>,
) -> Result<Json<Value>, ApiError> {
    let seconds = body.seconds.unwrap_or(3.0).clamp(0.25, 30.0);
    let recordings = state.recordings.clone();
    let clip = tokio::task::spawn_blocking(move || recordings.test_clip(seconds))
        .await
        .map_err(|e| ApiError::Internal(format!("audio_test_failed: {e}")))?;
    match clip {
        Ok(clip) => Ok(Json(json!(clip))),
        Err(error) => Err(ApiError::BadRequest(error)),
    }
}

pub(crate) async fn get_hits(
    State(state): State<AppState>,
    Query(filter): Query<hit_store::HitFilter>,
) -> Json<Value> {
    let limit = filter.limit.unwrap_or(100).clamp(1, 500);
    let offset = filter.offset.unwrap_or(0);
    let mut filter = filter;
    filter.limit = Some(limit);
    filter.offset = Some(offset);
    let (hits, total) = hit_store::list_hits(&state.analytics_db_path, &filter);
    Json(json!({
        "hits": hits,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
}

pub(crate) async fn get_hit(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    hit_store::get_hit(&state.analytics_db_path, &key)
        .map(|hit| Json(json!(hit)))
        .ok_or_else(|| ApiError::NotFound("hit_not_found".to_string()))
}

pub(crate) async fn get_hit_audio(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let hit = hit_store::get_hit(&state.analytics_db_path, &key)
        .ok_or_else(|| ApiError::NotFound("hit_not_found".to_string()))?;
    let file_name = hit
        .recording_file
        .ok_or_else(|| ApiError::NotFound("recording_not_found".to_string()))?;
    let path = state
        .recordings
        .audio_path(&file_name)
        .ok_or_else(|| ApiError::NotFound("recording_not_found".to_string()))?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::NotFound("recording_file_missing".to_string()))?;

    // Single-range support is enough for `<audio>` and for seeking in every
    // browser Bearpaw runs in. A multi-range request falls back to the whole
    // file, which is a valid response to a Range request (200, not 206).
    let total = bytes.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_single_range(value, total));

    let (status, body, content_range, content_length) = match range {
        Some((start, end)) => {
            let slice = bytes[start as usize..=(end as usize)].to_vec();
            let length = slice.len();
            (
                StatusCode::PARTIAL_CONTENT,
                Body::from(slice),
                Some(format!("bytes {start}-{end}/{total}")),
                length,
            )
        }
        None => (StatusCode::OK, Body::from(bytes), None, total as usize),
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers_mut = response.headers_mut();
    headers_mut.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    headers_mut.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers_mut.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers_mut.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&content_length.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    if let Some(content_range) = content_range {
        headers_mut.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&content_range)
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
    }
    Ok(response)
}

/// `bytes=start-end`, clamped to the file. Returns `None` for anything this
/// handler does not understand, which the caller answers with the full file.
fn parse_single_range(value: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        // Suffix range: the last N bytes.
        let suffix: u64 = end.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }
    let start: u64 = start.trim().parse().ok()?;
    if start >= total {
        return None;
    }
    let end = match end.trim() {
        "" => total - 1,
        raw => raw.parse::<u64>().ok()?.min(total - 1),
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

#[derive(Deserialize)]
pub(crate) struct TranscriptEdit {
    transcript: String,
}

/// The user's correction. The machine output is preserved in
/// `transcript_original` by `hit_store::edit_transcript`, so this is never
/// destructive.
pub(crate) async fn patch_transcript(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<TranscriptEdit>,
) -> Result<Json<Value>, ApiError> {
    let text = body.transcript.trim().to_string();
    if text.chars().count() > 20_000 {
        return Err(ApiError::BadRequest("transcript_too_long".to_string()));
    }
    let hit = hit_store::get_hit(&state.analytics_db_path, &key)
        .ok_or_else(|| ApiError::NotFound("hit_not_found".to_string()))?;
    let Some(uid) = hit.hit_uid else {
        return Err(ApiError::Conflict("hit_has_no_transcript".to_string()));
    };
    if !hit_store::edit_transcript(&state.analytics_db_path, &uid, &text) {
        return Err(ApiError::Internal("transcript_write_failed".to_string()));
    }
    hit_store::get_hit(&state.analytics_db_path, &uid)
        .map(|hit| Json(json!(hit)))
        .ok_or_else(|| ApiError::Internal("transcript_write_failed".to_string()))
}

#[derive(Deserialize, Default)]
pub(crate) struct TranscribeRequest {
    /// Retranscribe even when a transcript already exists.
    #[serde(default)]
    force: bool,
}

pub(crate) async fn post_transcribe(
    State(state): State<AppState>,
    Path(key): Path<String>,
    body: Option<Json<TranscribeRequest>>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let hit = hit_store::get_hit(&state.analytics_db_path, &key)
        .ok_or_else(|| ApiError::NotFound("hit_not_found".to_string()))?;
    let Some(uid) = hit.hit_uid else {
        return Err(ApiError::Conflict("hit_has_no_recording".to_string()));
    };
    if !matches!(hit.recording_status.as_str(), "ready" | "partial") {
        return Err(ApiError::Conflict("recording_not_available".to_string()));
    }
    let force = body.map(|Json(b)| b.force).unwrap_or(false);
    // Queue first, then mark pending: a job that starts instantly would set
    // `processing` and this write would put it back to `pending`.
    state.transcription.retry(&uid, force);
    hit_store::set_transcript_status(&state.analytics_db_path, &uid, "pending", None, false);
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "hit_uid": uid, "transcript_status": "pending" })),
    ))
}

pub(crate) async fn delete_recording(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let hit = hit_store::get_hit(&state.analytics_db_path, &key)
        .ok_or_else(|| ApiError::NotFound("hit_not_found".to_string()))?;
    let Some(uid) = hit.hit_uid else {
        return Err(ApiError::NotFound("recording_not_found".to_string()));
    };
    let Some(file_name) = hit.recording_file else {
        return Err(ApiError::NotFound("recording_not_found".to_string()));
    };
    let deleted_audio = state.recordings.delete_audio_file(&file_name);
    hit_store::mark_recording_deleted(&state.analytics_db_path, &uid);
    // "Delete recording and its transcript" is the contract of this endpoint.
    // The hit row itself stays, so the history keeps its frequency, channel and
    // timestamps even with the media gone.
    hit_store::clear_transcript(&state.analytics_db_path, &uid);
    Ok(Json(json!({
        "hit_uid": uid,
        "deleted_audio": deleted_audio,
        "transcript_cleared": true,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_file_range_is_accepted() {
        assert_eq!(parse_single_range("bytes=0-99", 100), Some((0, 99)));
        assert_eq!(parse_single_range("bytes=0-", 100), Some((0, 99)));
    }

    #[test]
    fn a_range_past_the_end_is_clamped_not_rejected() {
        assert_eq!(parse_single_range("bytes=90-500", 100), Some((90, 99)));
    }

    #[test]
    fn a_suffix_range_reads_from_the_end() {
        assert_eq!(parse_single_range("bytes=-20", 100), Some((80, 99)));
    }

    #[test]
    fn a_start_past_the_end_is_not_a_range() {
        assert_eq!(parse_single_range("bytes=100-200", 100), None);
    }

    #[test]
    fn malformed_and_multi_ranges_fall_back_to_the_whole_file() {
        for value in ["bytes=0-1,3-4", "items=0-1", "bytes=abc-def", "bytes=5-1"] {
            assert_eq!(parse_single_range(value, 100), None, "{value}");
        }
    }

    #[test]
    fn an_empty_file_has_no_range() {
        assert_eq!(parse_single_range("bytes=0-", 0), None);
    }
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use crate::api::{insert_analytics_hit, router, ActivityHit};
    use crate::state::ScannerMode;
    use axum::body::Body;
    use axum::http::{HeaderMap, Method, Request};
    use std::path::PathBuf;
    use tower::ServiceExt;

    fn seeded() -> (AppState, PathBuf) {
        let state = crate::api::default_state();
        let root = state.recordings.root();
        std::fs::create_dir_all(&root).expect("recordings root");
        insert_analytics_hit(
            &state.analytics_db_path,
            &ActivityHit {
                id: "1".to_string(),
                timestamp: 1_700_000_000.0,
                frequency: 161.1,
                channel: Some(4),
                alpha_tag: Some("AMTRAK".to_string()),
                rssi: 30,
                duration: 1.0,
                modulation: "NFM".to_string(),
                mode: ScannerMode::Scan,
                bank: Some(1),
                session_id: "s".to_string(),
                ended_at: 1_700_000_001.0,
                scanner_id: Some("BC125AT".to_string()),
                hit_uid: Some("h-api".to_string()),
                recording_status: "ready".to_string(),
                recording_file: Some("h-api.wav".to_string()),
                recording_duration: Some(1.0),
                recording_bytes: Some(8),
                link_confidence: Some("exact".to_string()),
                ..Default::default()
            },
        );
        std::fs::write(root.join("h-api.wav"), b"RIFFdata").expect("audio file");
        (state, root)
    }

    async fn call(
        state: &AppState,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let request = match body {
            Some(body) => Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
            None => Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        };
        let response = router(state.clone())
            .oneshot(request)
            .await
            .expect("response");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec();
        (status, headers, bytes)
    }

    fn json(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("json body")
    }

    #[tokio::test]
    async fn search_finds_the_hit_and_an_unknown_detail_is_404() {
        let (state, _root) = seeded();
        let (status, _, body) = call(&state, Method::GET, "/api/v1/hits?search=amtrak", None).await;
        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        assert_eq!(body["total"], 1);
        assert_eq!(body["hits"][0]["hit_uid"], "h-api");

        let (status, _, body) = call(&state, Method::GET, "/api/v1/hits/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&body)["error"], "hit_not_found");
    }

    #[tokio::test]
    async fn audio_is_served_whole_and_by_range() {
        let (state, _root) = seeded();
        let (status, headers, body) =
            call(&state, Method::GET, "/api/v1/hits/h-api/audio", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "audio/wav");
        assert_eq!(headers["accept-ranges"], "bytes");
        assert_eq!(body, b"RIFFdata");

        let request = Request::builder()
            .uri("/api/v1/hits/h-api/audio")
            .header("range", "bytes=0-3")
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], "bytes 0-3/8");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"RIFF");
    }

    #[tokio::test]
    async fn a_correction_keeps_the_machine_text_and_clearing_keeps_the_hit() {
        let (state, root) = seeded();
        let (status, _, body) = call(
            &state,
            Method::PATCH,
            "/api/v1/hits/h-api/transcript",
            Some(json!({ "transcript": "Amtrak 215 clear at Zoo" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(json(&body)["transcript"], "Amtrak 215 clear at Zoo");

        let (status, _, _) =
            call(&state, Method::DELETE, "/api/v1/hits/h-api/recording", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!root.join("h-api.wav").exists());
        let (status, _, body) = call(&state, Method::GET, "/api/v1/hits/h-api", None).await;
        assert_eq!(status, StatusCode::OK);
        let hit = json(&body);
        assert_eq!(hit["recording_status"], "deleted");
        assert_eq!(hit["frequency"], 161.1);
        assert!(hit["transcript"].is_null());
    }

    #[tokio::test]
    async fn transcribing_a_hit_without_audio_conflicts() {
        let state = crate::api::default_state();
        insert_analytics_hit(
            &state.analytics_db_path,
            &ActivityHit {
                id: "2".to_string(),
                timestamp: 1_700_000_000.0,
                frequency: 160.725,
                hit_uid: Some("h-none".to_string()),
                recording_status: "none".to_string(),
                ..Default::default()
            },
        );
        let (status, _, body) = call(
            &state,
            Method::POST,
            "/api/v1/hits/h-none/transcribe",
            Some(json!({ "force": false })),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(json(&body)["error"], "recording_not_available");
    }

    #[tokio::test]
    async fn a_recorded_hit_can_be_queued_for_transcription() {
        let (state, _root) = seeded();
        let (status, _, body) = call(
            &state,
            Method::POST,
            "/api/v1/hits/h-api/transcribe",
            Some(json!({ "force": true })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(json(&body)["hit_uid"], "h-api");
        // The worker runs on its own thread; stop it so the test does not leave
        // a thread behind racing the next test's database.
        state.transcription.stop();
    }
}
