//! ZCode (z.ai) session parser
//!
//! Parses the ZCode v2 CLI usage database at `~/.zcode/cli/db/db.sqlite`
//! (`model_usage` LEFT JOIN `session`). Only the v2 SQLite store is ported
//! from upstream; the legacy `~/.zcode/projects/*/*.jsonl` transcript reader
//! is not (see UPSTREAM.md).

use super::utils::{back_anchor_timestamp, file_modified_timestamp_ms, open_readonly_sqlite};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::TokenBreakdown;
use rusqlite::Connection;
use std::path::Path;

const CLIENT_ID: &str = "zcode";
const PROVIDER_ID: &str = "zhipu";
const UNKNOWN_MODEL: &str = "glm-5.2";

/// Subtract `overlap` out of `value`, clamping both operands to non-negative
/// and never going below zero. Mirrors `gemini.rs`'s `subtract_cached_overlap`
/// but takes the pre-summed overlap directly, since ZCode's `input_tokens`
/// absorbs two separate buckets (cache read + cache write) rather than one.
fn subtract_overlap(value: i64, overlap: i64) -> i64 {
    let value = value.max(0);
    let overlap = overlap.max(0);
    value.saturating_sub(overlap.min(value))
}

/// ZCode's `model_usage` rows report `input_tokens` and `output_tokens` as
/// cache/reasoning-inclusive: `input_tokens` already contains
/// `cache_read_input_tokens` + `cache_creation_input_tokens`, and
/// `output_tokens` already contains `reasoning_tokens`. Tokscale's
/// `TokenBreakdown` instead expects five non-overlapping buckets, so passing
/// the raw columns straight through double-counts cache and reasoning in
/// `TokenBreakdown::total()`.
///
/// When a reported `total` is available we use it to detect which shape
/// we're looking at, mirroring `gemini.rs`'s
/// `normalize_gemini_session_input_and_cache`: if the reported total matches
/// the cache/reasoning-inclusive sum (`input + output`) rather than the fully
/// additive sum (`input + output + cache_read + cache_write + reasoning`),
/// the row is inclusive and needs the overlap subtracted.
///
/// When `total` is absent, the shape can't be detected here, so the raw
/// input/output are returned unchanged; `parse_zcode_sqlite`'s legacy-schema
/// fallback has separate evidence about its shape and applies its own
/// subtraction. Returns `(net_input, net_output)`.
fn normalize_zcode_input_and_output(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    total: Option<i64>,
) -> (i64, i64) {
    let input = input.max(0);
    let output = output.max(0);
    let cache_overlap = cache_read.max(0).saturating_add(cache_write.max(0));
    let reasoning = reasoning.max(0);

    let Some(total) = total.map(|value| value.max(0)) else {
        return (input, output);
    };

    let inclusive_total = input.saturating_add(output);
    let exclusive_total = inclusive_total
        .saturating_add(cache_overlap)
        .saturating_add(reasoning);

    if (cache_overlap > 0 || reasoning > 0) && total == inclusive_total && total != exclusive_total
    {
        return (
            subtract_overlap(input, cache_overlap),
            subtract_overlap(output, reasoning),
        );
    }

    (input, output)
}

const MODERN_QUERY: &str = r#"
    SELECT
        mu.id,
        NULLIF(mu.session_id, ''),
        NULLIF(mu.turn_id, ''),
        NULLIF(mu.model_id, ''),
        mu.started_at,
        mu.completed_at,
        mu.duration_ms,
        mu.input_tokens,
        mu.output_tokens,
        mu.reasoning_tokens,
        mu.cache_read_input_tokens,
        mu.cache_creation_input_tokens,
        mu.computed_total_tokens,
        NULLIF(mu.agent, ''),
        NULLIF(mu.mode, ''),
        NULLIF(s.directory, ''),
        NULLIF(s.path, '')
    FROM model_usage mu
    LEFT JOIN session s ON s.id = mu.session_id
    WHERE COALESCE(mu.input_tokens, 0)
        + COALESCE(mu.output_tokens, 0)
        + COALESCE(mu.reasoning_tokens, 0)
        + COALESCE(mu.cache_read_input_tokens, 0)
        + COALESCE(mu.cache_creation_input_tokens, 0) > 0
    ORDER BY COALESCE(mu.completed_at, mu.started_at, 0), mu.id
"#;

const LEGACY_QUERY: &str = r#"
    SELECT
        mu.id,
        NULLIF(mu.session_id, ''),
        NULLIF(mu.turn_id, ''),
        NULLIF(mu.model_id, ''),
        mu.started_at,
        mu.completed_at,
        mu.duration_ms,
        mu.input_tokens,
        mu.output_tokens,
        mu.reasoning_tokens,
        mu.cache_read_input_tokens,
        mu.cache_creation_input_tokens,
        NULL,
        NULLIF(mu.agent, ''),
        NULLIF(mu.mode, ''),
        NULL,
        NULL
    FROM model_usage mu
    WHERE COALESCE(mu.input_tokens, 0)
        + COALESCE(mu.output_tokens, 0)
        + COALESCE(mu.reasoning_tokens, 0)
        + COALESCE(mu.cache_read_input_tokens, 0)
        + COALESCE(mu.cache_creation_input_tokens, 0) > 0
    ORDER BY COALESCE(mu.completed_at, mu.started_at, 0), mu.id
"#;

/// Stream ZCode usage rows to `sink` without retaining per-row messages,
/// turn ids, or per-turn maps in memory.
///
/// `is_turn_start` needs the earliest start-anchored timestamp per turn, but
/// rows arrive ordered by completion. A bounded two-pass scan stages only
/// `(turn_id, timestamp, ordinal)` in a file-backed scratch table, then the
/// second pass marks the winner per turn. Ties keep the first row in query
/// order, matching the old `HashMap` pass's strict `<` replacement.
pub(crate) fn visit_zcode_sqlite(
    db_path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let Some(conn) = open_readonly_sqlite(db_path) else {
        return Ok(());
    };

    let fallback_timestamp = file_modified_timestamp_ms(db_path);

    // Probe the `computed_total_tokens` column directly instead of inferring
    // legacy schema from the modern query failing to prepare: the modern query
    // also LEFT JOINs the `session` table, so it can fail for reasons
    // unrelated to the column's existence (e.g. a missing or renamed session
    // table). Conflating those would send modern-schema rows with NULL totals
    // through the unconditional subtraction below (potential undercount)
    // instead of the safe pass-through.
    let is_legacy_schema = super::utils::prepare_source_sqlite(
        &conn,
        "SELECT computed_total_tokens FROM model_usage LIMIT 1",
        &[
            (
                "model_usage",
                &[
                    "id",
                    "session_id",
                    "turn_id",
                    "model_id",
                    "started_at",
                    "completed_at",
                    "duration_ms",
                    "input_tokens",
                    "output_tokens",
                    "reasoning_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                    "computed_total_tokens",
                    "agent",
                    "mode",
                ],
            ),
            ("session", &["id", "directory", "path"]),
        ],
    )
    .is_err();

    // A query that prepares is the one this schema supports; only a prepare
    // failure means "try the older spelling". A database that understands
    // neither is not a ZCode usage store.
    let active_query = [MODERN_QUERY, LEGACY_QUERY]
        .into_iter()
        .find(|query| super::utils::prepare_source_sqlite(&conn, query, ZCODE_SCHEMA).is_ok());
    let Some(active_query) = active_query else {
        return Ok(());
    };

    let scratch = open_zcode_scratch()?;

    // Both passes read under one explicit read transaction so a concurrent
    // writer between passes cannot change the winning started_at or make a
    // pass-two row miss its pass-one lookup. Read-only handle: BEGIN (not
    // BEGIN IMMEDIATE); ROLLBACK is best-effort since the txn ends at return.
    super::utils::observe_sqlite(conn.execute_batch("BEGIN;"))
        .map_err(|error| format!("starting ZCode source read transaction: {error}"))?;
    let visit = (|| -> Result<(), String> {
        let mut upsert = scratch
            .prepare(
                "INSERT INTO temp.turn_earliest(turn_id, ts, ord) VALUES (?1, ?2, ?3)
             ON CONFLICT(turn_id) DO UPDATE SET ts = excluded.ts, ord = excluded.ord
             WHERE excluded.ts < turn_earliest.ts
                OR (excluded.ts = turn_earliest.ts AND excluded.ord < turn_earliest.ord)",
            )
            .map_err(|error| format!("creating ZCode scratch statement: {error}"))?;
        // First pass: earliest start-anchored timestamp (and winning ordinal)
        // per turn over exactly the rows the second pass will publish. Row
        // callbacks stay unobserved here: decoding happens again in pass two,
        // so only pass two may fail the generation.
        let mut ordinal: i64 = 0;
        {
            let mut stmt = super::utils::prepare_source_sqlite(&conn, active_query, ZCODE_SCHEMA)
                .map_err(|_| "ZCode source query failed".to_string())?;
            let mapped = super::utils::observe_sqlite(stmt.query_map([], zcode_row))
                .map_err(|_| "ZCode source query failed".to_string())?;
            for row in mapped {
                match row {
                    Ok(row) => {
                        let Some((message, turn_id)) =
                            zcode_message_from_row(row, fallback_timestamp, is_legacy_schema)
                        else {
                            continue;
                        };
                        if let Some(turn_id) = turn_id {
                            upsert
                                .execute(rusqlite::params![turn_id, message.timestamp, ordinal])
                                .map_err(|error| {
                                    format!("writing ZCode scratch earliest turn: {error}")
                                })?;
                        }
                        ordinal += 1;
                    }
                    Err(error) if zcode_row_failed_hard(&error) => {
                        return Err(format!("ZCode source read failed: {error}"));
                    }
                    Err(_) => {}
                }
            }
        }

        #[cfg(test)]
        zcode_between_passes_hook();

        // Second pass: rebuild each published message in the same stable order
        // and mark the earliest-started row per turn.
        let mut lookup = scratch
            .prepare("SELECT ts, ord FROM temp.turn_earliest WHERE turn_id = ?1")
            .map_err(|error| format!("creating ZCode scratch lookup: {error}"))?;
        ordinal = 0;
        {
            let mut stmt = super::utils::prepare_source_sqlite(&conn, active_query, ZCODE_SCHEMA)
                .map_err(|_| "ZCode source query failed".to_string())?;
            let mapped = super::utils::observe_sqlite(stmt.query_map([], zcode_row))
                .map_err(|_| "ZCode source query failed".to_string())?;
            for row in super::utils::source_rows(mapped) {
                match row {
                    Ok(row) => {
                        let Some((mut message, turn_id)) =
                            zcode_message_from_row(row, fallback_timestamp, is_legacy_schema)
                        else {
                            continue;
                        };
                        if let Some(turn_id) = turn_id.as_deref() {
                            let (winner_ts, winner_ord): (i64, i64) = lookup
                                .query_row([turn_id], |query_row| {
                                    Ok((query_row.get(0)?, query_row.get(1)?))
                                })
                                .map_err(|error| {
                                    format!("reading ZCode scratch earliest turn: {error}")
                                })?;
                            if message.timestamp == winner_ts && ordinal == winner_ord {
                                message.is_turn_start = true;
                            }
                        }
                        ordinal += 1;
                        sink(message)?;
                    }
                    Err(error) if zcode_row_failed_hard(&error) => {
                        return Err(format!("ZCode source read failed: {error}"));
                    }
                    Err(_) => {}
                }
            }
        }
        Ok(())
    })();
    let _ = conn.execute_batch("ROLLBACK;");
    visit
}

const ZCODE_SCHEMA: &[(&str, &[&str])] = &[
    (
        "model_usage",
        &[
            "id",
            "session_id",
            "turn_id",
            "model_id",
            "started_at",
            "completed_at",
            "duration_ms",
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "computed_total_tokens",
            "agent",
            "mode",
        ],
    ),
    ("session", &["id", "directory", "path"]),
];

fn zcode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ZcodeUsageRow> {
    Ok(ZcodeUsageRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        turn_id: row.get(2)?,
        model_id: row.get(3)?,
        started_at: row.get(4)?,
        completed_at: row.get(5)?,
        duration_ms: row.get(6)?,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        reasoning_tokens: row.get(9)?,
        cache_read_input_tokens: row.get(10)?,
        cache_creation_input_tokens: row.get(11)?,
        computed_total_tokens: row.get(12)?,
        agent: row.get(13)?,
        mode: row.get(14)?,
        session_directory: row.get(15)?,
        session_path: row.get(16)?,
    })
}

/// Build the published message for one source row, or `None` when the row
/// carries no tokens (the old `tokens.total() == 0` skip). The returned
/// turn id is already filtered for empty ids.
fn zcode_message_from_row(
    row: ZcodeUsageRow,
    fallback_timestamp: i64,
    is_legacy_schema: bool,
) -> Option<(UnifiedMessage, Option<String>)> {
    let session_id = row.session_id.unwrap_or_else(|| "unknown".to_string());
    let model_id = row
        .model_id
        .as_deref()
        .map(canonicalize_model)
        .unwrap_or_else(|| UNKNOWN_MODEL.to_string());
    let timestamp = resolve_zcode_timestamp(
        row.started_at,
        row.completed_at,
        row.duration_ms,
        fallback_timestamp,
    );

    let raw_input = row.input_tokens.unwrap_or(0);
    let raw_output = row.output_tokens.unwrap_or(0);
    let raw_cache_read = row.cache_read_input_tokens.unwrap_or(0);
    let raw_cache_write = row.cache_creation_input_tokens.unwrap_or(0);
    let raw_reasoning = row.reasoning_tokens.unwrap_or(0);

    let (net_input, net_output) = match row.computed_total_tokens {
        Some(total) => normalize_zcode_input_and_output(
            raw_input,
            raw_output,
            raw_cache_read,
            raw_cache_write,
            raw_reasoning,
            Some(total),
        ),
        // When `computed_total_tokens` is NULL, distinguish two cases:
        // 1. Legacy schema (column doesn't exist): unconditionally subtract,
        //    since every sampled row in a real ZCode database is confirmed
        //    cache/reasoning-inclusive.
        // 2. Modern schema but this row's value is NULL: can't detect shape,
        //    so pass through unchanged (the normalize function's default when
        //    total is None). Subtracting unconditionally here would undercount
        //    rows that are already cache-exclusive.
        None if is_legacy_schema => (
            subtract_overlap(
                raw_input,
                raw_cache_read.max(0).saturating_add(raw_cache_write.max(0)),
            ),
            subtract_overlap(raw_output, raw_reasoning),
        ),
        None => normalize_zcode_input_and_output(
            raw_input,
            raw_output,
            raw_cache_read,
            raw_cache_write,
            raw_reasoning,
            None,
        ),
    };

    let tokens = TokenBreakdown {
        input: net_input,
        output: net_output,
        cache_read: raw_cache_read.max(0),
        cache_write: raw_cache_write.max(0),
        cache_write_1h: 0,
        reasoning: raw_reasoning.max(0),
    };

    if tokens.total() == 0 {
        return None;
    }

    let agent = row
        .agent
        .as_deref()
        .or(row.mode.as_deref())
        .map(str::to_string);
    let mut message = UnifiedMessage::new_with_agent(
        CLIENT_ID,
        model_id,
        PROVIDER_ID,
        session_id,
        timestamp,
        tokens,
        0.0,
        agent,
    );
    message.dedup_key = Some(format!("zcode-sqlite:{}", row.id));
    message.duration_ms = row.duration_ms.filter(|duration| *duration > 0);

    let workspace_root = row.session_directory.or(row.session_path);
    let workspace_key = workspace_root.as_deref().and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    message.set_workspace(workspace_key, workspace_label);

    let turn_id = row.turn_id.filter(|id| !id.is_empty());
    Some((message, turn_id))
}

/// Conversion errors keep the old skip-row semantics; execution errors abort
/// the generation so the publisher never treats a partial prefix as success.
fn zcode_row_failed_hard(error: &rusqlite::Error) -> bool {
    !matches!(
        error,
        rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::Utf8Error(..)
            | rusqlite::Error::InvalidColumnIndex(..)
            | rusqlite::Error::InvalidColumnName(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::QueryReturnedNoRows
    )
}

// Deterministic test-only barrier between the two source passes. Runs after
// the first pass statement is dropped and before the second SELECT starts,
// so a test writer mutates the source exactly where the reported race lives.
// Production builds compile it out; never sleeps.
#[cfg(test)]
thread_local! {
    static ZCODE_BETWEEN_PASSES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn zcode_between_passes_hook() {
    if let Some(hook) = ZCODE_BETWEEN_PASSES.take() {
        hook();
    }
}

/// File-backed scratch for per-turn earliest markers. Only the TEMP schema
/// holds tables so pages spill to disk instead of the heap.
fn open_zcode_scratch() -> Result<Connection, String> {
    let scratch =
        Connection::open_in_memory().map_err(|error| format!("opening ZCode scratch: {error}"))?;
    scratch
        .execute_batch(
            "PRAGMA temp_store=FILE;
             PRAGMA temp.cache_size=-64;
             PRAGMA temp.cache_spill=ON;
             PRAGMA mmap_size=0;",
        )
        .map_err(|error| format!("configuring ZCode scratch: {error}"))?;
    let disk_temp: bool = scratch
        .query_row(
            "SELECT (SELECT temp_store FROM pragma_temp_store) = 1
             AND sqlite_compileoption_used('TEMP_STORE=3') = 0",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("checking ZCode scratch storage: {error}"))?;
    if !disk_temp {
        return Err("ZCode scratch requires file-backed SQLite TEMP storage".into());
    }
    scratch
        .execute_batch(
            "CREATE TEMP TABLE turn_earliest (
                 turn_id TEXT PRIMARY KEY,
                 ts INTEGER NOT NULL,
                 ord INTEGER NOT NULL
             ) WITHOUT ROWID;",
        )
        .map_err(|error| format!("creating ZCode scratch: {error}"))?;
    Ok(scratch)
}

pub fn parse_zcode_sqlite(db_path: &Path) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    match visit_zcode_sqlite(db_path, &mut |message| {
        messages.push(message);
        Ok(())
    }) {
        Ok(()) => messages,
        Err(error) => {
            tracing::warn!(db_path = %db_path.display(), %error, "ZCode source read failed; discarding partial generation");
            Vec::new()
        }
    }
}

/// Resolve the anchor timestamp for a `model_usage` row.
///
/// Prefers `started_at` when it's a positive epoch, since it anchors the
/// message at the call's actual start, matching `duration_ms`'s own
/// start-to-end span. When `started_at` is missing or non-positive, falls
/// back to `completed_at`, back-calculating the start anchor from
/// `completed_at - duration_ms` when a positive `duration_ms` is available
/// — anchoring at `completed_at` directly would make sessionize()'s
/// `[timestamp, timestamp + duration_ms]` span project forward past the
/// actual completion into phantom idle time. The back-calculation is guarded
/// against a non-positive result (which sessionize() silently drops) by
/// falling back to the unadjusted `completed_at`.
fn resolve_zcode_timestamp(
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    fallback_timestamp: i64,
) -> i64 {
    if let Some(started) = started_at.filter(|value| *value > 0) {
        return started;
    }
    match completed_at {
        Some(completed) => match duration_ms.filter(|duration| *duration > 0) {
            Some(duration) => back_anchor_timestamp(completed, duration),
            None => completed,
        },
        None => fallback_timestamp,
    }
}

struct ZcodeUsageRow {
    id: String,
    session_id: Option<String>,
    turn_id: Option<String>,
    model_id: Option<String>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    computed_total_tokens: Option<i64>,
    agent: Option<String>,
    mode: Option<String>,
    session_directory: Option<String>,
    session_path: Option<String>,
}

/// Canonicalize ZCode model ids. ZCode reports GLM model names in various
/// forms (e.g. "glm-5.2", "GLM-5.2", "glm-5-turbo"); normalize to lowercase
/// canonical form for pricing lookup.
fn canonicalize_model(model: &str) -> String {
    model.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    fn create_zcode_sqlite_db(dir: &TempDir) -> std::path::PathBuf {
        let db_path = dir.path().join("db.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE model_usage (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                turn_id TEXT,
                model_id TEXT,
                started_at INTEGER,
                completed_at INTEGER,
                duration_ms INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                reasoning_tokens INTEGER,
                cache_read_input_tokens INTEGER,
                cache_creation_input_tokens INTEGER,
                computed_total_tokens INTEGER,
                agent TEXT,
                mode TEXT
            );
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT,
                path TEXT
            );
            "#,
        )
        .unwrap();
        db_path
    }

    #[test]
    fn test_canonicalize_model() {
        assert_eq!(canonicalize_model("GLM-5.2"), "glm-5.2");
        assert_eq!(canonicalize_model("glm-5-turbo"), "glm-5-turbo");
    }

    #[test]
    fn test_parse_zcode_sqlite_model_usage() {
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO session (id, directory, path) VALUES (?1, ?2, ?3)",
            params!["sess_1", "/Users/alice/work/demo", "/Users/alice/work/demo"],
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, turn_id, model_id, started_at, completed_at,
                duration_ms, input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens, agent, mode
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            "#,
            params![
                "usage_1",
                "sess_1",
                "turn_1",
                "GLM-5.2",
                1_782_718_000_000_i64,
                1_782_718_001_000_i64,
                1000_i64,
                100_i64,
                20_i64,
                5_i64,
                7_i64,
                3_i64,
                120_i64,
                "zcode-agent",
                "yolo",
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.client, "zcode");
        assert_eq!(msg.provider_id, "zhipu");
        assert_eq!(msg.model_id, "glm-5.2");
        assert_eq!(msg.session_id, "sess_1");
        // Timestamp anchors to `started_at` (the call's start), not
        // `completed_at` (the call's end).
        assert_eq!(msg.timestamp, 1_782_718_000_000_i64);
        assert_eq!(msg.duration_ms, Some(1000));
        assert_eq!(msg.tokens.input, 90);
        assert_eq!(msg.tokens.output, 15);
        assert_eq!(msg.tokens.reasoning, 5);
        assert_eq!(msg.tokens.cache_read, 7);
        assert_eq!(msg.tokens.cache_write, 3);
        assert_eq!(msg.agent.as_deref(), Some("zcode-agent"));
        assert_eq!(msg.workspace_key.as_deref(), Some("/Users/alice/work/demo"));
        assert_eq!(msg.workspace_label.as_deref(), Some("demo"));
        assert!(msg.is_turn_start);
        assert_eq!(msg.dedup_key.as_deref(), Some("zcode-sqlite:usage_1"));
    }

    #[test]
    fn test_parse_zcode_sqlite_marks_only_first_request_per_turn() {
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        for (id, completed_at) in [("usage_1", 1_000_i64), ("usage_2", 2_000_i64)] {
            conn.execute(
                r#"
                INSERT INTO model_usage (
                    id, session_id, turn_id, model_id, completed_at,
                    input_tokens, output_tokens
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    id,
                    "sess_1",
                    "turn_1",
                    "glm-5.2",
                    completed_at,
                    10_i64,
                    1_i64
                ],
            )
            .unwrap();
        }

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].is_turn_start);
        assert!(!messages[1].is_turn_start);
    }

    #[test]
    fn test_model_usage_timestamp_is_start_anchored() {
        // `model_usage` records both `started_at` and `completed_at` for a
        // call, plus an explicit `duration_ms`. Anchoring the message timestamp
        // at `completed_at` would make sessionize()'s
        // `[timestamp, timestamp + duration_ms]` span project forward past the
        // actual completion into phantom idle time. The parser must prefer
        // `started_at`.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, turn_id, model_id, started_at, completed_at,
                duration_ms, input_tokens, output_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                "usage_1",
                "sess_1",
                "turn_1",
                "glm-5.2",
                1_782_718_000_000_i64,
                1_782_718_005_000_i64,
                5000_i64,
                10_i64,
                1_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].timestamp, 1_782_718_000_000_i64,
            "timestamp must anchor at started_at, not completed_at"
        );
        assert_eq!(
            messages[0].duration_ms,
            Some(5000),
            "duration_ms must still span from start to completion"
        );
    }

    #[test]
    fn test_model_usage_missing_started_at_back_calculates_from_completed_at() {
        // When `started_at` is NULL but `completed_at` and a positive
        // `duration_ms` are present, the row must not stay end-anchored at
        // `completed_at`. Back-calculate the start anchor from
        // `completed_at - duration_ms` instead.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, turn_id, model_id, completed_at,
                duration_ms, input_tokens, output_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                "usage_1",
                "sess_1",
                "turn_1",
                "glm-5.2",
                1_782_718_005_000_i64,
                5000_i64,
                10_i64,
                1_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].timestamp, 1_782_718_000_000_i64,
            "timestamp must be back-calculated from completed_at - duration_ms when started_at is missing"
        );
        assert_eq!(messages[0].duration_ms, Some(5000));
    }

    #[test]
    fn test_parse_zcode_sqlite_cache_inclusive_normalization() {
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, model_id, completed_at,
                input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                "usage_cache_incl",
                "sess_cache",
                "glm-5.2",
                1_000_i64,
                100_i64,
                50_i64,
                10_i64,
                80_i64,
                5_i64,
                150_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.tokens.input, 15);
        assert_eq!(msg.tokens.output, 40);
        assert_eq!(msg.tokens.cache_read, 80);
        assert_eq!(msg.tokens.cache_write, 5);
        assert_eq!(msg.tokens.reasoning, 10);
        assert_eq!(msg.tokens.total(), 150);
    }

    #[test]
    fn test_parse_zcode_sqlite_legacy_schema_subtracts_unconditionally() {
        // True legacy schema: no `computed_total_tokens` column (and no
        // `session` table), so the column probe and the modern query both
        // fail and the legacy fallback runs with is_legacy_schema=true.
        // Every row must then take the unconditional-subtraction branch.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("db.sqlite");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE model_usage (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                turn_id TEXT,
                model_id TEXT,
                started_at INTEGER,
                completed_at INTEGER,
                duration_ms INTEGER,
                input_tokens INTEGER,
                output_tokens INTEGER,
                reasoning_tokens INTEGER,
                cache_read_input_tokens INTEGER,
                cache_creation_input_tokens INTEGER,
                agent TEXT,
                mode TEXT
            );
            "#,
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, model_id, completed_at,
                input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                "usage_legacy",
                "sess_legacy",
                "glm-5.2",
                1_000_i64,
                100_i64,
                50_i64,
                10_i64,
                80_i64,
                5_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.tokens.input, 15);
        assert_eq!(msg.tokens.output, 40);
        assert_eq!(msg.tokens.cache_read, 80);
        assert_eq!(msg.tokens.cache_write, 5);
        assert_eq!(msg.tokens.reasoning, 10);
        assert_eq!(msg.tokens.total(), 150);
    }

    #[test]
    fn test_parse_zcode_sqlite_modern_schema_null_total_passes_through() {
        // Modern schema (computed_total_tokens column exists) but this row's
        // value is NULL: the shape can't be detected, so input/output must
        // pass through unchanged rather than being unconditionally subtracted.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, model_id, completed_at,
                input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
            "#,
            params![
                "usage_null_total",
                "sess_null",
                "glm-5.2",
                1_000_i64,
                100_i64,
                50_i64,
                10_i64,
                80_i64,
                5_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.tokens.input, 100);
        assert_eq!(msg.tokens.output, 50);
        assert_eq!(msg.tokens.cache_read, 80);
        assert_eq!(msg.tokens.cache_write, 5);
        assert_eq!(msg.tokens.reasoning, 10);
    }

    #[test]
    fn test_parse_zcode_sqlite_cache_exclusive_preserved() {
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO model_usage (
                id, session_id, model_id, completed_at,
                input_tokens, output_tokens, reasoning_tokens,
                cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                "usage_cache_excl",
                "sess_excl",
                "claude-sonnet-5",
                1_000_i64,
                20_i64,
                30_i64,
                5_i64,
                80_i64,
                10_i64,
                145_i64,
            ],
        )
        .unwrap();

        let messages = parse_zcode_sqlite(&db_path);

        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.tokens.input, 20);
        assert_eq!(msg.tokens.output, 30);
        assert_eq!(msg.tokens.cache_read, 80);
        assert_eq!(msg.tokens.cache_write, 10);
        assert_eq!(msg.tokens.reasoning, 5);
        assert_eq!(msg.tokens.total(), 145);
    }

    #[test]
    fn test_visit_zcode_marks_earliest_start_despite_completion_order() {
        // Regression: rows arrive ordered by completed_at, but is_turn_start
        // must flag the earliest-STARTED request per turn.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        // Later-started row completes first, so it sorts first in query
        // order; the earlier-started row must still win the flag.
        for (id, started, completed) in [
            ("usage_late_start", 2_000_i64, 2_100_i64),
            ("usage_early_start", 1_000_i64, 3_000_i64),
        ] {
            conn.execute(
                r#"
                INSERT INTO model_usage (
                    id, session_id, turn_id, model_id, started_at, completed_at,
                    input_tokens, output_tokens
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![id, "sess_1", "turn_1", "glm-5.2", started, completed, 10_i64, 1_i64],
            )
            .unwrap();
        }
        drop(conn);

        let mut messages = Vec::new();
        visit_zcode_sqlite(&db_path, &mut |message| {
            messages.push(message);
            Ok(())
        })
        .unwrap();

        assert_eq!(messages.len(), 2);
        // Stable query order (by completion): late-start row first.
        assert_eq!(messages[0].timestamp, 2_000);
        assert_eq!(messages[1].timestamp, 1_000);
        assert!(!messages[0].is_turn_start);
        assert!(messages[1].is_turn_start);
    }

    #[test]
    fn test_visit_zcode_earliest_start_tie_keeps_first_in_query_order() {
        // Tie behavior: equal earliest timestamps keep the first row in
        // query order (completion, then id), matching the old strict-`<`.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        for id in ["usage_a", "usage_b"] {
            conn.execute(
                r#"
                INSERT INTO model_usage (
                    id, session_id, turn_id, model_id, started_at, completed_at,
                    input_tokens, output_tokens
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![id, "sess_1", "turn_1", "glm-5.2", 1_000_i64, 2_000_i64, 10_i64, 1_i64],
            )
            .unwrap();
        }
        drop(conn);

        let mut messages = Vec::new();
        visit_zcode_sqlite(&db_path, &mut |message| {
            messages.push(message);
            Ok(())
        })
        .unwrap();

        assert_eq!(messages.len(), 2);
        assert!(messages[0].is_turn_start);
        assert!(!messages[1].is_turn_start);
    }

    #[test]
    fn test_visit_zcode_sink_error_aborts_without_further_publish() {
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        let conn = Connection::open(&db_path).unwrap();
        for id in ["usage_1", "usage_2"] {
            conn.execute(
                r#"
                INSERT INTO model_usage (
                    id, session_id, model_id, completed_at,
                    input_tokens, output_tokens
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![id, "sess_1", "glm-5.2", 1_000_i64, 10_i64, 1_i64],
            )
            .unwrap();
        }
        drop(conn);

        let mut published = 0;
        let result = visit_zcode_sqlite(&db_path, &mut |_| {
            published += 1;
            Err("boom".to_string())
        });

        assert_eq!(result, Err("boom".to_string()));
        assert_eq!(published, 1);
    }

    #[test]
    fn test_visit_zcode_snapshot_stable_under_concurrent_writer() {
        // Deterministic (no sleeps): the WAL writer fires from the
        // between-passes hook — after the first pass statement is dropped
        // but before the second SELECT starts. That is exactly where the
        // reported race lives (a writer inside the first sink call runs
        // after the second SELECT already fixed its snapshot and cannot
        // catch the race). Under the explicit read transaction pass two
        // must still publish the two snapshot rows with original
        // timestamps and the precomputed turn flag on usage_2; without
        // the transaction the divergent pass-two scan sees usage_3 and
        // the moved started_at.
        let dir = TempDir::new().unwrap();
        let db_path = create_zcode_sqlite_db(&dir);
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            for (id, started, completed) in [
                ("usage_1", 2_000_i64, 2_100_i64),
                ("usage_2", 1_000_i64, 3_000_i64),
            ] {
                conn.execute(
                    r#"
                    INSERT INTO model_usage (
                        id, session_id, turn_id, model_id, started_at, completed_at,
                        input_tokens, output_tokens
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    "#,
                    params![id, "sess_1", "turn_1", "glm-5.2", started, completed, 10_i64, 1_i64],
                )
                .unwrap();
            }
        }

        let hook_path = db_path.clone();
        ZCODE_BETWEEN_PASSES.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let writer = Connection::open(&hook_path).unwrap();
                writer
                    .execute(
                        "UPDATE model_usage SET started_at = 500 WHERE id = 'usage_2'",
                        [],
                    )
                    .unwrap();
                writer
                    .execute(
                        r#"
                    INSERT INTO model_usage (
                        id, session_id, turn_id, model_id, started_at, completed_at,
                        input_tokens, output_tokens
                    ) VALUES ('usage_3', 'sess_1', 'turn_1', 'glm-5.2', 100, 150, 10, 1)
                    "#,
                        [],
                    )
                    .unwrap();
            }) as Box<dyn FnOnce()>);
        });

        let mut messages = Vec::new();
        visit_zcode_sqlite(&db_path, &mut |message| {
            messages.push(message);
            Ok(())
        })
        .unwrap();
        ZCODE_BETWEEN_PASSES.with(|slot| assert!(slot.borrow_mut().take().is_none()));

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("zcode-sqlite:usage_1")
        );
        assert_eq!(messages[0].timestamp, 2_000);
        assert!(!messages[0].is_turn_start);
        assert_eq!(
            messages[1].dedup_key.as_deref(),
            Some("zcode-sqlite:usage_2")
        );
        assert_eq!(messages[1].timestamp, 1_000);
        assert!(messages[1].is_turn_start);
        // Prove the hook fired and the writer committed: the snapshot hid
        // the mutation, but the file now holds it.
        let check = Connection::open(&db_path).unwrap();
        let started: i64 = check
            .query_row(
                "SELECT started_at FROM model_usage WHERE id = 'usage_2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started, 500);
        let extra: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM model_usage WHERE id = 'usage_3'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(extra, 1);
    }
}
