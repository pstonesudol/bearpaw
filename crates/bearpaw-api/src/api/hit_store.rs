//! Queries for the parts of a hit that are not pure activity: its recording,
//! its transcript, and the searchable history view.
//!
//! `api::mod` keeps the hot-path hit insert (one statement, on the poll thread).
//! Everything here is either off the poll thread (recording/transcription
//! workers) or on a request, so it can afford to open its own connection and
//! keep the query readable.

use std::path::{Component, Path, PathBuf};

use rusqlite::{params, OptionalExtension, Row};
use serde::Deserialize;

use super::{open_sqlite, ActivityHit, ScannerMode};

/// Select list and mapper live together so a new column cannot be added to one
/// and forgotten in the other.
const HIT_COLUMNS: &str = "id, timestamp, frequency, channel, alpha_tag, modulation, rssi, \
     duration, mode, bank, session_id, ended_at, scanner_id, hit_uid, recording_path, \
     recording_status, link_confidence, recording_duration, recording_bytes, recording_error, \
     transcript, transcript_original, transcript_status, transcript_engine, transcript_error, \
     transcript_attempts, updated_at";

pub(crate) fn row_to_hit(row: &Row<'_>) -> rusqlite::Result<ActivityHit> {
    let rssi: i64 = row.get("rssi")?;
    let duration: Option<f64> = row.get("duration")?;
    let timestamp: f64 = row.get("timestamp")?;
    let ended_at: Option<f64> = row.get("ended_at")?;
    let mode: String = row.get("mode")?;
    let transcript_original: Option<String> = row.get("transcript_original")?;
    let transcript: Option<String> = row.get("transcript")?;
    Ok(ActivityHit {
        id: row.get::<_, i64>("id")?.to_string(),
        timestamp,
        frequency: row.get("frequency")?,
        channel: row.get("channel")?,
        alpha_tag: row.get("alpha_tag")?,
        rssi: rssi as u8,
        duration: duration.unwrap_or(0.0),
        modulation: row.get("modulation")?,
        mode: ScannerMode::from_str(&mode),
        bank: row.get("bank")?,
        session_id: row.get("session_id")?,
        ended_at: ended_at.unwrap_or(timestamp),
        scanner_id: row.get("scanner_id")?,
        hit_uid: row.get("hit_uid")?,
        recording_status: row
            .get::<_, Option<String>>("recording_status")?
            .unwrap_or_else(|| "none".to_string()),
        recording_file: row.get("recording_path")?,
        recording_duration: row.get("recording_duration")?,
        recording_bytes: row.get("recording_bytes")?,
        recording_error: row.get("recording_error")?,
        link_confidence: row.get("link_confidence")?,
        transcript: transcript.clone(),
        transcript_original: transcript_original.filter(|original| {
            // Only interesting when it differs from the displayed text.
            transcript.as_deref() != Some(original.as_str())
        }),
        transcript_status: row
            .get::<_, Option<String>>("transcript_status")?
            .unwrap_or_else(|| "none".to_string()),
        transcript_engine: row.get("transcript_engine")?,
        transcript_error: row.get("transcript_error")?,
        transcript_attempts: row
            .get::<_, Option<i64>>("transcript_attempts")?
            .unwrap_or(0) as u32,
        updated_at: row.get("updated_at")?,
    })
}

/// Record the outcome of a finished (or failed) segment.
///
/// Returns `false` when no row matched, which means the hit was below the
/// activity minimum or the database is unavailable. The caller must then delete
/// the file: an audio file with no row is an orphan by definition.
pub(crate) fn set_recording_outcome(
    db_path: &str,
    hit_uid: &str,
    file_name: Option<&str>,
    status: &str,
    confidence: Option<&str>,
    duration_secs: Option<f64>,
    bytes: Option<i64>,
    error: Option<&str>,
) -> bool {
    let Some(conn) = open_sqlite(db_path) else {
        return false;
    };
    let now = super::epoch_now();
    conn.execute(
        "UPDATE scan_hits SET recording_path = ?2, recording_status = ?3, \
         link_confidence = ?4, recording_duration = ?5, recording_bytes = ?6, \
         recording_error = ?7, updated_at = ?8 WHERE hit_uid = ?1",
        params![
            hit_uid,
            file_name,
            status,
            confidence,
            duration_secs,
            bytes,
            error,
            now
        ],
    )
    .map(|changed| changed > 0)
    .unwrap_or(false)
}

/// A row whose audio was deleted on purpose. The hit and any transcript stay.
pub(crate) fn mark_recording_deleted(db_path: &str, hit_uid: &str) -> Option<String> {
    let conn = open_sqlite(db_path)?;
    let previous: Option<String> = conn
        .query_row(
            "SELECT recording_path FROM scan_hits WHERE hit_uid = ?1",
            params![hit_uid],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten();
    let _ = conn.execute(
        "UPDATE scan_hits SET recording_path = NULL, recording_status = 'deleted', \
         recording_bytes = NULL, updated_at = ?2 WHERE hit_uid = ?1",
        params![hit_uid, super::epoch_now()],
    );
    previous
}

pub(crate) fn clear_transcript(db_path: &str, hit_uid: &str) {
    let Some(conn) = open_sqlite(db_path) else {
        return;
    };
    let _ = conn.execute(
        "UPDATE scan_hits SET transcript = NULL, transcript_original = NULL, \
         transcript_status = 'none', transcript_error = NULL, updated_at = ?2 \
         WHERE hit_uid = ?1",
        params![hit_uid, super::epoch_now()],
    );
}

/// Store a machine transcript. `original` is only written when no correction
/// exists yet, so retranscription never overwrites the user's edit either.
pub(crate) fn set_transcript_completed(
    db_path: &str,
    hit_uid: &str,
    text: &str,
    engine: &str,
) -> bool {
    let Some(conn) = open_sqlite(db_path) else {
        return false;
    };
    let now = super::epoch_now();
    // `transcript_original` always becomes the latest machine output; the
    // displayed `transcript` is replaced only when the user has not corrected
    // it. A correction is the user's work — retranscribing must not silently
    // discard it, and the fresh machine text is still available under
    // "Machine transcript". Both `CASE` arms read the OLD row values in SQLite,
    // so the comparison is against the pre-update state.
    conn.execute(
        "UPDATE scan_hits SET \
         transcript = CASE \
             WHEN transcript_original IS NOT NULL AND transcript IS NOT transcript_original \
             THEN transcript ELSE ?2 END, \
         transcript_original = ?2, \
         transcript_status = 'completed', \
         transcript_engine = ?3, transcript_error = NULL, transcript_attempts = 0, \
         updated_at = ?4 \
         WHERE hit_uid = ?1",
        params![hit_uid, text, engine, now],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

pub(crate) fn set_transcript_status(
    db_path: &str,
    hit_uid: &str,
    status: &str,
    error: Option<&str>,
    increment_attempts: bool,
) {
    let Some(conn) = open_sqlite(db_path) else {
        return;
    };
    let attempts_sql = if increment_attempts {
        "transcript_attempts = COALESCE(transcript_attempts, 0) + 1,"
    } else {
        ""
    };
    let _ = conn.execute(
        &format!(
            "UPDATE scan_hits SET transcript_status = ?2, transcript_error = ?3, \
             {attempts_sql} updated_at = ?4 WHERE hit_uid = ?1"
        ),
        params![hit_uid, status, error, super::epoch_now()],
    );
}

/// The user corrected a transcript. The machine text is preserved separately.
pub(crate) fn edit_transcript(db_path: &str, hit_uid: &str, text: &str) -> bool {
    let Some(conn) = open_sqlite(db_path) else {
        return false;
    };
    conn.execute(
        "UPDATE scan_hits SET transcript = ?2, \
         transcript_original = COALESCE(transcript_original, transcript), \
         transcript_status = 'completed', updated_at = ?3 WHERE hit_uid = ?1",
        params![hit_uid, text, super::epoch_now()],
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct HitFilter {
    pub search: Option<String>,
    pub channel: Option<u16>,
    pub frequency: Option<f64>,
    pub frequency_min: Option<f64>,
    pub frequency_max: Option<f64>,
    pub start_time: Option<f64>,
    pub end_time: Option<f64>,
    pub recording_status: Option<String>,
    pub transcript_status: Option<String>,
    pub has_recording: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub scanner_id: Option<String>,
}

pub(crate) fn list_hits(db_path: &str, filter: &HitFilter) -> (Vec<ActivityHit>, usize) {
    let Some(conn) = open_sqlite(db_path) else {
        return (Vec::new(), 0);
    };

    // LIKE rather than FTS5: the bundled SQLite does not guarantee the FTS5
    // module, and a migration that fails on a user's machine for a search
    // optimisation is a bad trade. `COALESCE` keeps NULL rows searchable on
    // their other fields. The escape character is applied to user input so a
    // literal '%' or '_' searches for itself.
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(search) = filter
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let pattern = format!("%{}%", escape_like(&search.to_lowercase()));
        clauses.push(
            "(LOWER(COALESCE(alpha_tag, '')) LIKE ? ESCAPE '\\' \
             OR LOWER(COALESCE(transcript, '')) LIKE ? ESCAPE '\\' \
             OR LOWER(COALESCE(CAST(frequency AS TEXT), '')) LIKE ? ESCAPE '\\')"
                .to_string(),
        );
        for _ in 0..3 {
            values.push(Box::new(pattern.clone()));
        }
    }
    if let Some(channel) = filter.channel {
        clauses.push("channel = ?".to_string());
        values.push(Box::new(channel as i64));
    }
    if let Some(frequency) = filter.frequency {
        // Frequencies are stored with more precision than a person types;
        // 1 kHz is finer than any channel spacing this scanner offers.
        clauses.push("ABS(frequency - ?) < 0.001".to_string());
        values.push(Box::new(frequency));
    }
    if let Some(min) = filter.frequency_min {
        clauses.push("frequency >= ?".to_string());
        values.push(Box::new(min));
    }
    if let Some(max) = filter.frequency_max {
        clauses.push("frequency <= ?".to_string());
        values.push(Box::new(max));
    }
    if let Some(start) = filter.start_time {
        clauses.push("timestamp >= ?".to_string());
        values.push(Box::new(start));
    }
    if let Some(end) = filter.end_time {
        clauses.push("timestamp <= ?".to_string());
        values.push(Box::new(end));
    }
    if let Some(status) = filter.recording_status.as_deref().filter(|s| !s.is_empty()) {
        clauses.push("COALESCE(recording_status, 'none') = ?".to_string());
        values.push(Box::new(status.to_string()));
    }
    if let Some(status) = filter
        .transcript_status
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        clauses.push("COALESCE(transcript_status, 'none') = ?".to_string());
        values.push(Box::new(status.to_string()));
    }
    if let Some(has_recording) = filter.has_recording {
        // Status, not "has a path": a `missing` or `deleted` row can still
        // carry the old path, and a user filtering for playable audio does not
        // want a file that is gone.
        clauses.push(if has_recording {
            "COALESCE(recording_status, 'none') IN ('ready', 'partial')".to_string()
        } else {
            "COALESCE(recording_status, 'none') NOT IN ('ready', 'partial')".to_string()
        });
    }
    if let Some(scanner) = filter.scanner_id.as_deref().filter(|s| !s.is_empty()) {
        // Unattributed history counts in a scoped view, matching the analytics
        // handlers: a strict equality filter emptied every pre-attribution
        // user's history the first time they upgraded.
        clauses.push("(scanner_id IS NULL OR UPPER(scanner_id) = UPPER(?))".to_string());
        values.push(Box::new(scanner.to_string()));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };

    let total: usize = conn
        .query_row(
            &format!("SELECT COUNT(*) FROM scan_hits{where_sql}"),
            rusqlite::params_from_iter(values.iter().map(|v| v.as_ref())),
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as usize;

    let limit = filter.limit.unwrap_or(100).clamp(1, 500) as i64;
    let offset = filter.offset.unwrap_or(0) as i64;
    let sql = format!(
        "SELECT {HIT_COLUMNS} FROM scan_hits{where_sql} ORDER BY timestamp DESC LIMIT {limit} OFFSET {offset}"
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(e) => {
            tracing::warn!(error = %e, "hit search query could not be prepared");
            return (Vec::new(), total);
        }
    };
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(values.iter().map(|v| v.as_ref())),
            row_to_hit,
        )
        .map(|rows| rows.flatten().collect::<Vec<_>>())
        .unwrap_or_default();
    (rows, total)
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Look a hit up by its durable uid, or by its numeric row id for rows that
/// predate recordings. Both are exposed to the UI; only the uid is stable
/// across restarts.
pub(crate) fn get_hit(db_path: &str, key: &str) -> Option<ActivityHit> {
    let conn = open_sqlite(db_path)?;
    conn.query_row(
        &format!("SELECT {HIT_COLUMNS} FROM scan_hits WHERE hit_uid = ?1"),
        params![key],
        row_to_hit,
    )
    .optional()
    .ok()
    .flatten()
    .or_else(|| {
        let id: i64 = key.parse().ok()?;
        conn.query_row(
            &format!("SELECT {HIT_COLUMNS} FROM scan_hits WHERE id = ?1"),
            params![id],
            row_to_hit,
        )
        .optional()
        .ok()
        .flatten()
    })
}

/// Any `processing` job at startup was interrupted by the last shutdown. The
/// audio is still there, so the honest state is "queued again", not "failed".
pub(crate) fn requeue_interrupted_transcriptions(db_path: &str) -> Vec<String> {
    let Some(conn) = open_sqlite(db_path) else {
        return Vec::new();
    };
    let _ = conn.execute(
        "UPDATE scan_hits SET transcript_status = 'pending' WHERE transcript_status = 'processing'",
        [],
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT hit_uid FROM scan_hits WHERE transcript_status = 'pending' AND hit_uid IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
            out.extend(rows.flatten());
        }
    }
    out
}

pub(crate) fn pending_transcription_uids(db_path: &str) -> Vec<String> {
    requeue_interrupted_transcriptions(db_path)
}

/// Does this hit have a playable file on disk? Used after a move or a restore
/// to correct rows that still claim a recording.
pub(crate) fn set_recording_missing(db_path: &str, hit_uid: &str) {
    let Some(conn) = open_sqlite(db_path) else {
        return;
    };
    let _ = conn.execute(
        "UPDATE scan_hits SET recording_status = 'missing', updated_at = ?2 \
         WHERE hit_uid = ?1 AND recording_path IS NOT NULL",
        params![hit_uid, super::epoch_now()],
    );
}

/// Hits whose recording never reached a terminal state, with whatever path (if
/// any) was recorded before the process died.
///
/// Separate from [`recording_rows`] because an interrupted hit usually has NO
/// `recording_path`: the file is named after the uid and the row is updated
/// only when the segment finishes. Selecting on `recording_path IS NOT NULL`
/// would silently skip exactly the rows this is meant to recover.
pub(crate) fn interrupted_recordings(db_path: &str) -> Vec<(String, Option<String>)> {
    let Some(conn) = open_sqlite(db_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT hit_uid, recording_path FROM scan_hits \
         WHERE COALESCE(recording_status, 'none') IN ('recording', 'pending') \
         AND hit_uid IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?))) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Rows that reference a recording file. Used by retention and the orphan
/// sweep; `None` means the hit predates recordings or has none.
pub(crate) fn recording_rows(db_path: &str) -> Vec<(String, String, String, f64)> {
    let Some(conn) = open_sqlite(db_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT hit_uid, recording_path, COALESCE(recording_status, 'none'), timestamp \
         FROM scan_hits WHERE recording_path IS NOT NULL AND hit_uid IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

/// Delete audio older than `retention_days` while keeping every hit row.
/// Returns `(files_deleted, rows_marked)`. A retention of `0` means "keep until
/// the hit itself ages out", not "delete now" — a zero that wipes the library
/// the moment it is typed is a trap, and the activity retention already decides
/// how long a hit lives.
pub(crate) fn apply_audio_retention(
    db_path: &str,
    root: &Path,
    retention_days: u32,
) -> (usize, usize) {
    if retention_days == 0 {
        return (0, 0);
    }
    let cutoff = super::epoch_now() - f64::from(retention_days) * 24.0 * 3600.0;
    let mut files = 0;
    let mut rows = 0;
    for (hit_uid, file, _status, timestamp) in recording_rows(db_path) {
        if timestamp >= cutoff {
            continue;
        }
        if let Some(path) = safe_join(root, &file) {
            if std::fs::remove_file(&path).is_ok() {
                files += 1;
            }
        }
        if mark_recording_deleted(db_path, &hit_uid).is_some() {
            rows += 1;
        }
    }
    (files, rows)
}

/// Join a stored (relative) recording reference to the configured root.
///
/// Absolute paths and `..` are refused: the reference comes from the database,
/// which is not a trust boundary, and a corrupted row must not be able to read
/// or delete a file outside the library.
pub(crate) fn safe_join(root: &Path, file_name: &str) -> Option<PathBuf> {
    let candidate = Path::new(file_name);
    if candidate.is_absolute() {
        return None;
    }
    if candidate
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{init_analytics_db, insert_analytics_hit, unique_test_db_path, ActivityHit};

    fn hit(uid: &str, freq: f64, channel: u16) -> ActivityHit {
        ActivityHit {
            id: String::new(),
            timestamp: 1_000.0,
            frequency: freq,
            channel: Some(channel),
            alpha_tag: Some("NORFOLK SOUTHERN".to_string()),
            rssi: 42,
            duration: 3.5,
            modulation: "NFM".to_string(),
            mode: ScannerMode::Scan,
            bank: Some(2),
            session_id: "s".to_string(),
            ended_at: 1_003.5,
            scanner_id: Some("BC125AT".to_string()),
            hit_uid: Some(uid.to_string()),
            ..Default::default()
        }
    }

    fn seed() -> (String, std::path::PathBuf) {
        let db = unique_test_db_path("hits.db");
        init_analytics_db(&db).expect("analytics migration");
        let root = std::env::temp_dir().join(format!(
            "bearpaw-hit-store-{}-{}",
            std::process::id(),
            crate::api::uuid_simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        insert_analytics_hit(&db, &hit("h-1", 160.725, 12));
        insert_analytics_hit(&db, &hit("h-2", 161.1, 13));
        (db, root)
    }

    #[test]
    fn a_recording_outcome_updates_the_matching_hit_only() {
        let (db, _root) = seed();
        assert!(set_recording_outcome(
            &db,
            "h-1",
            Some("h-1.wav"),
            "ready",
            Some("exact"),
            Some(4.2),
            Some(134_400),
            None
        ));
        let one = get_hit(&db, "h-1").expect("row");
        let two = get_hit(&db, "h-2").expect("row");
        assert_eq!(one.recording_status, "ready");
        assert_eq!(one.recording_file.as_deref(), Some("h-1.wav"));
        assert_eq!(one.link_confidence.as_deref(), Some("exact"));
        assert_eq!(two.recording_status, "none");
    }

    #[test]
    fn an_outcome_for_an_unknown_hit_reports_no_match() {
        let (db, _root) = seed();
        assert!(!set_recording_outcome(
            &db,
            "missing",
            Some("missing.wav"),
            "ready",
            None,
            None,
            None,
            None
        ));
    }

    #[test]
    fn search_matches_alpha_tags_transcripts_and_frequencies() {
        let (db, _root) = seed();
        set_transcript_completed(&db, "h-2", "Amtrak 215 clear at Zoo", "whisper-cli");

        let by_tag = list_hits(
            &db,
            &HitFilter {
                search: Some("norfolk".into()),
                ..Default::default()
            },
        );
        assert_eq!(by_tag.0.len(), 2);

        let by_transcript = list_hits(
            &db,
            &HitFilter {
                search: Some("zoo".into()),
                ..Default::default()
            },
        );
        assert_eq!(by_transcript.0.len(), 1);
        assert_eq!(by_transcript.0[0].hit_uid.as_deref(), Some("h-2"));

        let by_frequency = list_hits(
            &db,
            &HitFilter {
                search: Some("161.1".into()),
                ..Default::default()
            },
        );
        assert_eq!(by_frequency.0.len(), 1);
    }

    #[test]
    fn filters_combine_and_report_a_total_beyond_the_page() {
        let (db, _root) = seed();
        let (page, total) = list_hits(
            &db,
            &HitFilter {
                has_recording: Some(false),
                limit: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(total, 2);
        assert_eq!(page.len(), 1);
    }

    #[test]
    fn a_literal_percent_in_a_search_is_not_a_wildcard() {
        let (db, _root) = seed();
        let (rows, _) = list_hits(
            &db,
            &HitFilter {
                search: Some("%".into()),
                ..Default::default()
            },
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn an_edit_preserves_the_machine_text() {
        let (db, _root) = seed();
        assert!(set_transcript_completed(
            &db,
            "h-1",
            "SEPTA 4-2-1 clear",
            "whisper-cli"
        ));
        assert!(edit_transcript(&db, "h-1", "SEPTA 421 clear"));
        let row = get_hit(&db, "h-1").unwrap();
        assert_eq!(row.transcript.as_deref(), Some("SEPTA 421 clear"));
        assert_eq!(
            row.transcript_original.as_deref(),
            Some("SEPTA 4-2-1 clear")
        );
    }

    /// Retranscribing must not silently throw away the user's correction. The
    /// new machine output belongs in `transcript_original`; the correction is
    /// the displayed text.
    #[test]
    fn retranscription_keeps_a_correction_and_refreshes_the_machine_text() {
        let (db, _root) = seed();
        set_transcript_completed(&db, "h-1", "machine one", "whisper-cli");
        edit_transcript(&db, "h-1", "human correction");
        set_transcript_completed(&db, "h-1", "machine two", "whisper-cli");
        let row = get_hit(&db, "h-1").unwrap();
        assert_eq!(row.transcript.as_deref(), Some("human correction"));
        assert_eq!(row.transcript_original.as_deref(), Some("machine two"));
    }

    #[test]
    fn retranscription_replaces_text_when_there_is_no_correction() {
        let (db, _root) = seed();
        set_transcript_completed(&db, "h-1", "machine one", "whisper-cli");
        set_transcript_completed(&db, "h-1", "machine two", "whisper-cli");
        let row = get_hit(&db, "h-1").unwrap();
        assert_eq!(row.transcript.as_deref(), Some("machine two"));
        // The stored original equals the displayed text, so the API hides it:
        // "Machine transcript" is only interesting when it differs.
        assert!(row.transcript_original.is_none());
    }

    #[test]
    fn interrupted_processing_jobs_are_requeued() {
        let (db, _root) = seed();
        set_transcript_status(&db, "h-1", "processing", None, false);
        let queued = requeue_interrupted_transcriptions(&db);
        assert_eq!(queued, vec!["h-1".to_string()]);
        assert_eq!(
            get_hit(&db, "h-1").unwrap().transcript_status,
            "pending".to_string()
        );
    }

    #[test]
    fn deleting_a_recording_keeps_the_hit_and_transcript() {
        let (db, _root) = seed();
        set_recording_outcome(
            &db,
            "h-1",
            Some("h-1.wav"),
            "ready",
            None,
            Some(1.0),
            Some(1),
            None,
        );
        set_transcript_completed(&db, "h-1", "clear", "whisper-cli");
        assert_eq!(
            mark_recording_deleted(&db, "h-1").as_deref(),
            Some("h-1.wav")
        );
        let row = get_hit(&db, "h-1").unwrap();
        assert_eq!(row.recording_status, "deleted");
        assert!(row.recording_file.is_none());
        assert_eq!(row.transcript.as_deref(), Some("clear"));
    }

    #[test]
    fn a_reference_cannot_escape_the_recordings_root() {
        let root = Path::new("/tmp/recordings");
        assert!(safe_join(root, "../etc/passwd").is_none());
        assert!(safe_join(root, "/etc/passwd").is_none());
        assert_eq!(
            safe_join(root, "sub/hit.wav").as_deref(),
            Some(Path::new("/tmp/recordings/sub/hit.wav"))
        );
        assert_eq!(
            safe_join(root, "hit.wav").as_deref(),
            Some(Path::new("/tmp/recordings/hit.wav"))
        );
    }

    #[test]
    fn retention_zero_keeps_audio_and_a_positive_window_marks_rows() {
        let (db, root) = seed();
        let file = root.join("h-1.wav");
        std::fs::write(&file, b"RIFF").unwrap();
        set_recording_outcome(
            &db,
            "h-1",
            Some("h-1.wav"),
            "ready",
            None,
            Some(1.0),
            Some(4),
            None,
        );

        assert_eq!(apply_audio_retention(&db, &root, 0), (0, 0));
        assert!(file.exists());

        // The seeded hit is from 1970, so any positive window expires it.
        let (files, rows) = apply_audio_retention(&db, &root, 30);
        assert_eq!((files, rows), (1, 1));
        assert!(!file.exists());
        assert_eq!(get_hit(&db, "h-1").unwrap().recording_status, "deleted");
        assert!(get_hit(&db, "h-1").is_some(), "the hit row survives");
    }
}
