//! MiMo Code session parser
//!
//! Parses messages from:
//! - SQLite database: ~/.local/share/mimocode/mimocode.db

use super::utils::open_readonly_sqlite;
use super::{
    normalize_opencode_agent_name, normalize_workspace_key, workspace_label_from_key,
    UnifiedMessage,
};
use crate::{provider_identity, TokenBreakdown};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// MiMo Code message structure (from SQLite data column)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MiMoCodeMessage {
    #[serde(default)]
    pub id: Option<String>,
    pub role: String,
    #[serde(rename = "modelID")]
    pub model_id: Option<String>,
    #[serde(rename = "providerID")]
    pub provider_id: Option<String>,
    pub cost: Option<f64>,
    pub tokens: Option<MiMoCodeTokens>,
    pub time: MiMoCodeTime,
    pub agent: Option<String>,
    pub mode: Option<String>,
    #[serde(default, deserialize_with = "deserialize_micode_path")]
    pub path: Option<MiMoCodePath>,
}

#[derive(Debug, Deserialize)]
pub struct MiMoCodePath {
    pub root: Option<String>,
}

fn deserialize_micode_path<'de, D>(deserializer: D) -> Result<Option<MiMoCodePath>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let root = value
        .get("root")
        .and_then(|root| root.as_str())
        .map(str::to_string);

    Ok(Some(MiMoCodePath { root }))
}

#[derive(Debug, Deserialize)]
pub struct MiMoCodeTokens {
    pub input: i64,
    pub output: i64,
    pub reasoning: Option<i64>,
    // MiMo assistant messages may omit `cache` (or its read/write); without a
    // default a missing field would fail deserialization and silently drop the
    // message in the parse loop's `Err(_) => continue` arm.
    #[serde(default)]
    pub cache: Option<MiMoCodeCache>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MiMoCodeCache {
    #[serde(default)]
    pub read: i64,
    #[serde(default)]
    pub write: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MiMoCodeTime {
    pub created: f64,
    pub completed: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
struct MiMoCodeSqliteFingerprint {
    created_bits: u64,
    completed_bits: Option<u64>,
    model_id: String,
    provider_id: String,
    input: i64,
    output: i64,
    reasoning: i64,
    cache_read: i64,
    cache_write: i64,
    cost_bits: u64,
    agent: Option<String>,
}

#[derive(Debug, Clone)]
struct MiMoCodeSqliteDedupState {
    has_embedded_message_id: bool,
    has_workspace_conflict: bool,
}

fn workspace_from_root(root: Option<&str>) -> (Option<String>, Option<String>) {
    let workspace_key = root.and_then(normalize_workspace_key);
    let workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
    (workspace_key, workspace_label)
}

fn set_workspace_from_root(message: &mut UnifiedMessage, root: Option<&str>) {
    let (workspace_key, workspace_label) = workspace_from_root(root);
    message.set_workspace(workspace_key, workspace_label);
}

fn merge_duplicate_workspace(
    message: &mut UnifiedMessage,
    state: &mut MiMoCodeSqliteDedupState,
    root: Option<&str>,
) {
    if state.has_workspace_conflict {
        return;
    }

    let (candidate_key, candidate_label) = workspace_from_root(root);
    match (message.workspace_key.as_deref(), candidate_key) {
        (None, Some(key)) => message.set_workspace(Some(key), candidate_label),
        (Some(existing), Some(candidate)) if existing != candidate => {
            state.has_workspace_conflict = true;
            message.set_workspace(None, None);
        }
        _ => {}
    }
}

fn encode_fingerprint(fingerprint: &MiMoCodeSqliteFingerprint) -> Result<Vec<u8>, String> {
    bincode::serialize(fingerprint).map_err(|error| error.to_string())
}

/// File-backed dedup scratch: one fingerprint row per distinct message, so the
/// source history never multiplies Rust heap. Rows carry first-seen order plus
/// the merge state that used to live beside the materialized Vec.
struct MiMoRows {
    database: Connection,
    next_ordinal: i64,
}

impl MiMoRows {
    fn open() -> Result<Self, String> {
        let database = Connection::open_in_memory()
            .map_err(|error| format!("opening MiMo scratch: {error}"))?;
        // Same small-cache policy as the source opener (main 256 KiB, TEMP 64
        // KiB, FILE-backed, no mmap): only TEMP tables use this pager.
        database
            .execute_batch(
                "PRAGMA cache_size=-256; PRAGMA temp.cache_size=-64; \
                 PRAGMA temp_store=FILE; PRAGMA mmap_size=0;",
            )
            .map_err(|error| format!("configuring MiMo scratch: {error}"))?;
        let disk_temp: bool = database
            .query_row(
                "SELECT (SELECT temp_store FROM pragma_temp_store) = 1 \
                 AND sqlite_compileoption_used('TEMP_STORE=3') = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("checking MiMo scratch storage: {error}"))?;
        if !disk_temp {
            return Err("MiMo scratch requires file-backed SQLite TEMP storage".into());
        }
        database
            .execute_batch(
                "CREATE TEMP TABLE rows (
                     fingerprint BLOB PRIMARY KEY,
                     ordinal INTEGER NOT NULL,
                     message BLOB NOT NULL,
                     has_embedded_message_id INTEGER NOT NULL,
                     has_workspace_conflict INTEGER NOT NULL
                 ) WITHOUT ROWID;
                 BEGIN;",
            )
            .map_err(|error| format!("creating MiMo scratch: {error}"))?;
        Ok(Self {
            database,
            next_ordinal: 0,
        })
    }

    fn fold(
        &mut self,
        fingerprint: &MiMoCodeSqliteFingerprint,
        has_message_id: bool,
        unified: UnifiedMessage,
        workspace_root: Option<&str>,
    ) -> Result<(), String> {
        let key = encode_fingerprint(fingerprint)?;
        let stored: Option<(Vec<u8>, bool, bool)> = self
            .database
            .query_row(
                "SELECT message, has_embedded_message_id, has_workspace_conflict \
                 FROM temp.rows WHERE fingerprint=?1",
                [key.as_slice()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, i64>(2)? != 0,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("reading MiMo scratch: {error}"))?;
        let Some((bytes, stored_has_id, stored_conflict)) = stored else {
            let ordinal = self.next_ordinal;
            self.next_ordinal += 1;
            let message = bincode::serialize(&unified).map_err(|error| error.to_string())?;
            self.database
                .execute(
                    "INSERT INTO temp.rows(fingerprint, ordinal, message, \
                     has_embedded_message_id, has_workspace_conflict) \
                     VALUES(?1, ?2, ?3, ?4, 0)",
                    rusqlite::params![key, ordinal, message, i64::from(has_message_id)],
                )
                .map_err(|error| format!("writing MiMo scratch: {error}"))?;
            return Ok(());
        };
        let mut current: UnifiedMessage =
            bincode::deserialize(&bytes).map_err(|error| error.to_string())?;
        let mut state = MiMoCodeSqliteDedupState {
            has_embedded_message_id: stored_has_id,
            has_workspace_conflict: stored_conflict,
        };
        let duplicate_authoritative = unified.has_authoritative_cost();
        if has_message_id && !state.has_embedded_message_id {
            state.has_embedded_message_id = true;
            current.dedup_key.clone_from(&unified.dedup_key);
        }
        merge_duplicate_workspace(&mut current, &mut state, workspace_root);
        if duplicate_authoritative {
            current.mark_provider_reported_cost();
        }
        let message = bincode::serialize(&current).map_err(|error| error.to_string())?;
        self.database
            .execute(
                "UPDATE temp.rows SET message=?2, has_embedded_message_id=?3, \
                 has_workspace_conflict=?4 WHERE fingerprint=?1",
                rusqlite::params![
                    key,
                    message,
                    i64::from(state.has_embedded_message_id),
                    i64::from(state.has_workspace_conflict)
                ],
            )
            .map_err(|error| format!("writing MiMo scratch: {error}"))?;
        Ok(())
    }

    fn emit(
        self,
        sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut statement = self
            .database
            .prepare("SELECT message FROM temp.rows ORDER BY ordinal")
            .map_err(|error| format!("reading MiMo scratch: {error}"))?;
        let mut rows = statement
            .query([])
            .map_err(|error| format!("reading MiMo scratch: {error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("reading MiMo scratch: {error}"))?
        {
            let bytes: Vec<u8> = row
                .get(0)
                .map_err(|error| format!("reading MiMo scratch: {error}"))?;
            let message: UnifiedMessage =
                bincode::deserialize(&bytes).map_err(|error| error.to_string())?;
            sink(message)?;
        }
        Ok(())
    }
}

/// Normalize an epoch `time.created`/`time.completed` value to milliseconds.
///
/// MiMo Code is expected to store epoch milliseconds (matching OpenCode), but
/// some builds/channels have been observed writing epoch *seconds*, which made
/// dates land ~1000x in the past (1970-era). A recent epoch is ~1.7e12 in ms
/// versus ~1.7e9 in seconds, so a value at/under the `1e12` threshold is
/// treated as seconds and scaled up. This mirrors `timestamp_secs_to_ms` in the
/// goose/hermes parsers.
fn micode_timestamp_to_ms(timestamp: f64) -> f64 {
    if timestamp > 1e12 {
        timestamp
    } else {
        timestamp * 1000.0
    }
}

fn micode_duration_ms(time: &MiMoCodeTime) -> Option<i64> {
    // Normalize both endpoints so a seconds/ms mismatch (or both-in-seconds)
    // still yields a millisecond duration rather than a value 1000x too small.
    let duration = micode_timestamp_to_ms(time.completed?) - micode_timestamp_to_ms(time.created);
    if duration.is_finite() && duration > 0.0 {
        Some(duration as i64)
    } else {
        None
    }
}

pub fn parse_micode_sqlite(db_path: &Path) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    // Compatibility collector: the sink never fails, so a visit error can only
    // come from scratch setup/fold/emit; drop the prefix like other wrappers.
    if visit_micode_sqlite(db_path, &mut |message| {
        messages.push(message);
        Ok(())
    })
    .is_err()
    {
        messages.clear();
    }
    messages
}

/// Stream one source database through `sink` without retaining its history in
/// Rust heap. Dedup state lives in a file-backed SQLite scratch table keyed by
/// the exact legacy fingerprint; only the current row plus one stored
/// candidate are materialized when a duplicate merges.
pub(crate) fn visit_micode_sqlite(
    db_path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let Some(conn) = open_readonly_sqlite(db_path) else {
        return Ok(());
    };

    let modern_query = r#"
        SELECT m.id, m.session_id, m.data, NULLIF(s.directory, '') AS workspace_root
        FROM message m
        LEFT JOIN session s ON s.id = m.session_id
        WHERE json_extract(m.data, '$.role') = 'assistant'
          AND json_extract(m.data, '$.tokens') IS NOT NULL
        ORDER BY m.id, m.session_id
    "#;

    let legacy_query = r#"
        SELECT m.id, m.session_id, m.data, NULL AS workspace_root
        FROM message m
        WHERE json_extract(m.data, '$.role') = 'assistant'
          AND json_extract(m.data, '$.tokens') IS NOT NULL
        ORDER BY m.id, m.session_id
    "#;

    let mut stmt = match super::utils::prepare_source_sqlite(
        &conn,
        modern_query,
        &[
            ("message", &["id", "session_id", "data"]),
            ("session", &["id", "directory"]),
        ],
    )
    .or_else(|_| {
        super::utils::prepare_source_sqlite(
            &conn,
            legacy_query,
            &[
                ("message", &["id", "session_id", "data"]),
                ("session", &["id", "directory"]),
            ],
        )
    }) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };

    let rows = match super::utils::observe_sqlite(stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let session_id: String = row.get(1)?;
        let data_json: String = row.get(2)?;
        let workspace_root: Option<String> = row.get(3)?;
        Ok((id, session_id, data_json, workspace_root))
    }))
    .map(super::utils::source_rows)
    {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    let mut scratch = MiMoRows::open()?;

    // Namespace ONLY the row-id fallback by the database. MiMo Code uses
    // channel-suffixed databases (mimocode.db and mimocode-<channel>.db), and a
    // mid-session channel switch can write the SAME message to both files. The
    // embedded message id is globally unique, so it must stay un-namespaced to
    // collapse those duplicates across files. SQLite rowids, by contrast, are
    // per-database and not globally unique, so the fallback path namespaces them
    // to avoid falsely merging two different messages that share a rowid.
    let db_namespace = db_path.to_string_lossy().to_string();

    for row_result in rows {
        let (row_id, session_id, data_json, row_workspace_root) = match row_result {
            Ok(r) => r,
            Err(_) => continue,
        };

        let mut bytes = data_json.into_bytes();
        let msg: MiMoCodeMessage = match simd_json::from_slice(&mut bytes) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if msg.role != "assistant" {
            continue;
        }

        let message_id = msg.id.clone();
        let embedded_workspace_root = msg
            .path
            .as_ref()
            .and_then(|path| path.root.as_deref())
            .map(str::to_string);

        let tokens = match msg.tokens {
            Some(t) => t,
            None => continue,
        };

        let model_id = match msg.model_id {
            Some(m) => m,
            None => continue,
        };

        let provider_id = msg.provider_id.unwrap_or_else(|| "unknown".to_string());
        let provider_id =
            provider_identity::canonical_provider(&provider_id).unwrap_or(provider_id);
        let agent_or_mode = msg.mode.or(msg.agent);
        let agent = agent_or_mode.map(|a| normalize_opencode_agent_name(&a));
        let input = tokens.input.max(0);
        let output = tokens.output.max(0);
        let reasoning = tokens.reasoning.unwrap_or(0).max(0);
        let cache = tokens.cache.unwrap_or_default();
        let cache_read = cache.read.max(0);
        let cache_write = cache.write.max(0);
        let provider_cost = msg.cost.filter(|cost| cost.is_finite() && *cost >= 0.0);
        let cost = provider_cost.unwrap_or(0.0);
        // Normalize epoch values to milliseconds up front so the timestamp, the
        // dedup fingerprint, and the duration all agree even when MiMo writes
        // seconds instead of milliseconds.
        let created_ms = micode_timestamp_to_ms(msg.time.created);
        let completed_ms = msg.time.completed.map(micode_timestamp_to_ms);
        let dedup_key = match message_id.clone() {
            // Embedded ids are globally unique: keep them un-namespaced so the
            // same message in mimocode.db and mimocode-<channel>.db collapses.
            Some(id) => id,
            // Rowids are per-database: namespace to avoid false cross-DB merges.
            None => format!("{db_namespace}:{row_id}"),
        };
        let fingerprint = MiMoCodeSqliteFingerprint {
            created_bits: created_ms.to_bits(),
            completed_bits: completed_ms.map(f64::to_bits),
            model_id: model_id.clone(),
            provider_id: provider_id.clone(),
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            cost_bits: cost.to_bits(),
            agent: agent.clone(),
        };

        let mut unified = UnifiedMessage::new_with_agent(
            "micode",
            model_id,
            provider_id,
            session_id,
            // `time.created` is normalized to epoch milliseconds above (MiMo
            // matches OpenCode's ms, but some channels write seconds);
            // UnifiedMessage's timestamp_to_date treats it as ms.
            created_ms as i64,
            TokenBreakdown {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
                cache_write_1h: 0,
            },
            cost,
            agent,
        );
        if provider_cost.is_some() {
            unified.mark_provider_reported_cost();
        }
        unified.duration_ms = micode_duration_ms(&msg.time);
        unified.dedup_key = Some(dedup_key);
        let workspace_root = row_workspace_root
            .as_deref()
            .or(embedded_workspace_root.as_deref());
        set_workspace_from_root(&mut unified, workspace_root);

        scratch.fold(&fingerprint, message_id.is_some(), unified, workspace_root)?;
    }

    scratch.emit(sink)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn create_micode_sqlite_db(db_path: &Path) -> Connection {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_parse_micode_sqlite_basic() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");

        let conn = create_micode_sqlite_db(&db_path);

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": {
                "input": 1000,
                "output": 500,
                "reasoning": 100,
                "cache": { "read": 200, "write": 50 }
            },
            "time": { "created": 1700000000000.0, "completed": 1700000001234.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_001", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "micode");
        assert_eq!(messages[0].model_id, "mimo-v2.5-pro");
        assert_eq!(messages[0].provider_id, "mimo");
        assert_eq!(messages[0].tokens.input, 1000);
        assert_eq!(messages[0].tokens.output, 500);
        assert_eq!(messages[0].tokens.reasoning, 100);
        assert_eq!(messages[0].tokens.cache_read, 200);
        assert_eq!(messages[0].tokens.cache_write, 50);
        assert!((messages[0].cost - 0.05).abs() < 1e-9);
        assert!(messages[0].has_authoritative_cost());
        assert_eq!(messages[0].duration_ms, Some(1234));
    }

    #[test]
    fn test_parse_micode_sqlite_skips_user_messages() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");

        let conn = create_micode_sqlite_db(&db_path);

        let user_msg = r#"{
            "role": "user",
            "modelID": "mimo-v2.5-pro",
            "time": { "created": 1700000000000.0 }
        }"#;

        let assistant_msg = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "tokens": { "input": 100, "output": 50, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
            "time": { "created": 1700000001000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_user", "ses_001", user_msg],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_assistant", "ses_001", assistant_msg],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        // This message carries no embedded JSON id, so the dedup key falls back
        // to the SQLite row id and is namespaced by the database path.
        assert!(messages[0]
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.ends_with(":msg_assistant")));
    }

    /// Regression: MiMo Code uses channel-suffixed databases (mimocode.db and
    /// mimocode-<channel>.db). A mid-session channel switch can write the SAME
    /// message (same embedded id) to both files. The embedded id must NOT be
    /// namespaced by the database, otherwise the cross-file dedup set produces
    /// two distinct keys and the message's cost + tokens get counted twice.
    #[test]
    fn embedded_message_id_is_not_namespaced_by_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_a = dir.path().join("mimocode.db");
        let db_b = dir.path().join("mimocode-beta.db");
        // Embedded JSON "id" is the globally unique message id.
        let msg = r#"{
            "id": "msg_shared",
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0 }
        }"#;
        // Different SQLite row ids prove the collapse is driven by the embedded
        // id (not the row id), exactly as a mid-session channel switch records.
        for (db, row_id) in [(&db_a, "row_a"), (&db_b, "row_b")] {
            let conn = create_micode_sqlite_db(db);
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![row_id, "ses_1", msg],
            )
            .unwrap();
            drop(conn);
        }

        let a = parse_micode_sqlite(&db_a);
        let b = parse_micode_sqlite(&db_b);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // Same embedded id across both channel databases yields IDENTICAL,
        // un-namespaced dedup keys, so a shared dedup set collapses the
        // duplicate to a single count.
        assert_eq!(a[0].dedup_key, Some("msg_shared".to_string()));
        assert_eq!(b[0].dedup_key, Some("msg_shared".to_string()));

        // Prove the collapse end-to-end with the same HashSet logic used by the
        // cross-file aggregation in lib.rs.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let kept: Vec<_> = a
            .into_iter()
            .chain(b)
            .filter(|m| m.dedup_key.as_ref().is_none_or(|k| seen.insert(k.clone())))
            .collect();
        assert_eq!(kept.len(), 1, "shared embedded id must be counted once");
    }

    /// Two DIFFERENT messages that happen to share a SQLite rowid across two
    /// databases (rowids are per-database, not globally unique) must NOT be
    /// collapsed by the cross-file dedup set. The row-id fallback path is
    /// namespaced by database precisely to keep them distinct.
    #[test]
    fn rowid_fallback_is_namespaced_by_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_a = dir.path().join("a.db");
        let db_b = dir.path().join("b.db");
        // No embedded "id" field -> the parser falls back to the SQLite rowid.
        let msg = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0 }
        }"#;
        for db in [&db_a, &db_b] {
            let conn = create_micode_sqlite_db(db);
            // Same SQLite row id ("id" column) in both databases. With no
            // embedded JSON id, the parser falls back to this row id.
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["row_shared", "ses_1", msg],
            )
            .unwrap();
            drop(conn);
        }

        let a = parse_micode_sqlite(&db_a);
        let b = parse_micode_sqlite(&db_b);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // Same row id ("row_shared") in two databases must yield DISTINCT,
        // db-namespaced keys so the two unrelated messages are not merged.
        assert_ne!(a[0].dedup_key, b[0].dedup_key);
        assert!(a[0].dedup_key.as_deref().unwrap().ends_with(":row_shared"));
        assert!(b[0].dedup_key.as_deref().unwrap().ends_with(":row_shared"));

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let kept: Vec<_> = a
            .into_iter()
            .chain(b)
            .filter(|m| m.dedup_key.as_ref().is_none_or(|k| seen.insert(k.clone())))
            .collect();
        assert_eq!(
            kept.len(),
            2,
            "rowid collisions across DBs must stay distinct"
        );
    }

    #[test]
    fn test_parse_micode_sqlite_negative_values_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");

        let conn = create_micode_sqlite_db(&db_path);

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": -0.05,
            "tokens": {
                "input": -100,
                "output": -50,
                "reasoning": -25,
                "cache": { "read": -200, "write": -10 }
            },
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_negative", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 0);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(messages[0].tokens.cache_read, 0);
        assert_eq!(messages[0].tokens.cache_write, 0);
        assert_eq!(messages[0].tokens.reasoning, 0);
        assert!(messages[0].cost >= 0.0);
        assert!(!messages[0].has_authoritative_cost());
    }

    #[test]
    fn test_parse_micode_sqlite_dedup_forked_history() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);

        let root_msg = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": {
                "input": 1000,
                "output": 500,
                "reasoning": 25,
                "cache": { "read": 200, "write": 50 }
            },
            "time": { "created": 1700000000000.0, "completed": 1700000000500.0 }
        }"#;

        let new_msg = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.08,
            "tokens": {
                "input": 1300,
                "output": 650,
                "reasoning": 40,
                "cache": { "read": 100, "write": 0 }
            },
            "time": { "created": 1700000001000.0, "completed": 1700000001500.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["root_row", "root_session", root_msg],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["fork_copy_row", "fork_session", root_msg],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["fork_new_row", "fork_session", new_msg],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 1000);
        assert_eq!(messages[1].tokens.input, 1300);
    }

    #[test]
    fn test_parse_micode_sqlite_workspace_from_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, directory) VALUES (?1, ?2)",
            rusqlite::params!["ses_001", "/Users/alice/micode-repo"],
        )
        .unwrap();

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": {
                "input": 1000,
                "output": 500,
                "reasoning": 0,
                "cache": { "read": 200, "write": 50 }
            },
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_ws", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("/Users/alice/micode-repo")
        );
        assert_eq!(messages[0].workspace_label.as_deref(), Some("micode-repo"));
    }

    #[test]
    fn test_parse_micode_sqlite_with_agent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "agent": "build",
            "cost": 0.05,
            "tokens": {
                "input": 1000,
                "output": 500,
                "reasoning": 100,
                "cache": { "read": 200, "write": 50 }
            },
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_agent", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].agent, Some("Build".to_string()));
    }

    /// Regression for PR #710: `time.created` was hard-assumed to be epoch
    /// milliseconds. If MiMo writes epoch *seconds*, the date landed ~1000x in
    /// the past (1970-era). A ms-valued and a seconds-valued `time.created` that
    /// denote the SAME instant must normalize to the same date and the same
    /// (millisecond-scale) timestamp. Without `micode_timestamp_to_ms`, the
    /// seconds variant would yield 1970-01-20 instead of 2023-11-14.
    #[test]
    fn test_parse_micode_sqlite_normalizes_seconds_and_milliseconds() {
        let dir = tempfile::tempdir().unwrap();
        let db_ms = dir.path().join("ms.db");
        let db_secs = dir.path().join("secs.db");

        // 1_700_000_000 s == 1_700_000_000_000 ms == 2023-11-14T22:13:20Z.
        let msg_ms = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0, "completed": 1700000001234.0 }
        }"#;
        // Same instant, expressed in epoch SECONDS (the bugged-input shape).
        let msg_secs = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000.0, "completed": 1700000001.234 }
        }"#;

        for (db, data) in [(&db_ms, msg_ms), (&db_secs, msg_secs)] {
            let conn = create_micode_sqlite_db(db);
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["msg_1", "ses_1", data],
            )
            .unwrap();
            drop(conn);
        }

        let ms = parse_micode_sqlite(&db_ms);
        let secs = parse_micode_sqlite(&db_secs);
        assert_eq!(ms.len(), 1);
        assert_eq!(secs.len(), 1);

        // Both inputs resolve to the SAME instant: identical timestamp (ms) and
        // identical, non-empty (i.e. not 1970-era-then-formatted) date.
        assert_eq!(ms[0].timestamp, 1_700_000_000_000);
        assert_eq!(secs[0].timestamp, 1_700_000_000_000);
        assert_eq!(ms[0].date, secs[0].date);
        assert!(!ms[0].date.is_empty());

        // Duration is in milliseconds for BOTH representations (~1234 ms), not
        // ~1 (which is what the seconds input would have produced unnormalized).
        assert_eq!(ms[0].duration_ms, Some(1234));
        assert_eq!(secs[0].duration_ms, Some(1234));
    }

    /// A non-object `path` field (e.g. a bare string instead of `{ "root": .. }`)
    /// must not crash deserialization or fail the whole message: the custom
    /// `deserialize_micode_path` extracts `root` defensively, leaving it `None`.
    /// The message must still parse and have no embedded-path workspace.
    #[test]
    fn test_parse_micode_sqlite_non_object_path_field() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);

        // `path` is a string, not an object — the deserializer's `.get("root")`
        // returns None rather than erroring, so the message survives.
        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 100, "output": 50 },
            "path": "/some/string/not/an/object",
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_badpath", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(
            messages.len(),
            1,
            "non-object path must not drop the message"
        );
        assert_eq!(messages[0].tokens.input, 100);
        // No usable root -> no workspace derived from the embedded path.
        assert_eq!(messages[0].workspace_key, None);
        assert_eq!(messages[0].workspace_label, None);
    }

    /// Legacy-query fallback: when the database has no `session` table, the
    /// modern query (which JOINs `session`) fails to prepare and the parser
    /// falls back to `legacy_query`. In that path `workspace_root` from the row
    /// is NULL, so the workspace must come from the message's EMBEDDED `path.root`.
    #[test]
    fn test_parse_micode_sqlite_legacy_fallback_embedded_path_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        // Note: create_micode_sqlite_db creates ONLY the `message` table, so the
        // modern query's `LEFT JOIN session` cannot prepare and we exercise the
        // legacy fallback.
        let conn = create_micode_sqlite_db(&db_path);

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": { "input": 100, "output": 50 },
            "path": { "root": "/Users/bob/embedded-repo" },
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_embedded", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        // Row workspace_root is NULL on the legacy path, so the embedded
        // `path.root` supplies the workspace.
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("/Users/bob/embedded-repo")
        );
        assert_eq!(
            messages[0].workspace_label.as_deref(),
            Some("embedded-repo")
        );
    }

    #[test]
    fn test_parse_micode_sqlite_missing_cache_defaults_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);

        // Assistant payload with no `cache` object at all — must parse (not be
        // dropped) with cache tokens defaulting to 0.
        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.05,
            "tokens": {
                "input": 1000,
                "output": 500,
                "reasoning": 100
            },
            "time": { "created": 1700000000000.0 }
        }"#;

        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_no_cache", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 1000);
        assert_eq!(messages[0].tokens.output, 500);
        assert_eq!(messages[0].tokens.cache_read, 0);
        assert_eq!(messages[0].tokens.cache_write, 0);
    }

    #[test]
    fn test_parse_micode_sqlite_zero_cost_is_provider_reported() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_micode.db");
        let conn = create_micode_sqlite_db(&db_path);

        let data_json = r#"{
            "role": "assistant",
            "modelID": "mimo-v2.5-pro",
            "providerID": "mimo",
            "cost": 0.0,
            "tokens": { "input": 100, "output": 50 },
            "time": { "created": 1700000000000.0 }
        }"#;
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["msg_zero_cost", "ses_001", data_json],
        )
        .unwrap();
        drop(conn);

        let messages = parse_micode_sqlite(&db_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost, 0.0);
        assert!(messages[0].has_authoritative_cost());
    }

    // A >1024-row duplicate fold pins mixed fingerprint/embedded-id/workspace
    // states: first-seen order wins, late embedded ids promote the dedup key,
    // conflicting workspaces clear, and provider-cost authority survives.
    #[test]
    fn visitor_folds_thousands_of_duplicates_with_exact_merge_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("many_dupes.db");
        let conn = create_micode_sqlite_db(&db_path);
        conn.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);")
            .unwrap();
        for (session, directory) in [
            ("ses_a", "/Users/alice/repo-a"),
            ("ses_b", "/Users/bob/repo-b"),
        ] {
            conn.execute(
                "INSERT INTO session (id, directory) VALUES (?1, ?2)",
                rusqlite::params![session, directory],
            )
            .unwrap();
        }
        // Fingerprint X: both copies share tokens/cost/timestamps so the exact
        // legacy fingerprint collides. The first copy has no embedded id and
        // ses_a workspace; the second carries a late embedded id, an explicit
        // zero cost (same 0.0 value, but provider-reported), and a conflicting
        // ses_b workspace.
        let x_plain = r#"{
            "role": "assistant", "modelID": "mimo-v2.5-pro", "providerID": "mimo",
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0 }
        }"#;
        let x_embedded = r#"{
            "id": "msg_x", "role": "assistant", "modelID": "mimo-v2.5-pro",
            "providerID": "mimo", "cost": 0.0,
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0 }
        }"#;
        // Fingerprint Y: distinct tokens/cost so it never collides with X.
        let y_plain = r#"{
            "role": "assistant", "modelID": "mimo-v2.5-pro", "providerID": "mimo",
            "tokens": { "input": 30, "output": 7 },
            "time": { "created": 1700000001000.0 }
        }"#;
        for index in 0..1100 {
            // Row ids sort Y before X (x_ > y_ would flip first-seen order), so
            // prefix them to keep insertion order == ORDER BY m.id order.
            let (row, session, data) = match index % 3 {
                0 => (format!("a_x_plain_{index:04}"), "ses_a", x_plain),
                1 => (format!("b_x_embedded_{index:04}"), "ses_b", x_embedded),
                _ => (format!("c_y_{index:04}"), "ses_a", y_plain),
            };
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![row, session, data],
            )
            .unwrap();
        }
        drop(conn);

        let mut visited = Vec::new();
        visit_micode_sqlite(&db_path, &mut |message| {
            visited.push(message);
            Ok(())
        })
        .unwrap();
        let materialized = parse_micode_sqlite(&db_path);
        assert_eq!(visited, materialized);
        assert_eq!(visited.len(), 2);
        // X folds first: late embedded id promotes the key, the ses_a/ses_b
        // conflict clears the workspace, and the duplicate's explicit zero cost
        // marks the survivor authoritative.
        assert_eq!(visited[0].dedup_key.as_deref(), Some("msg_x"));
        assert_eq!(visited[0].workspace_key, None);
        assert!(visited[0].has_authoritative_cost());
        assert_eq!(visited[1].tokens.input, 30);
        assert!(!visited[1].has_authoritative_cost());
    }

    /// Callback failures abort the fold: no partial sink publication counts as
    /// success and the error surfaces to the caller.
    #[test]
    fn visitor_callback_failure_propagates_without_success() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sink_failure.db");
        let conn = create_micode_sqlite_db(&db_path);
        let data_json = r#"{
            "role": "assistant", "modelID": "mimo-v2.5-pro", "providerID": "mimo",
            "tokens": { "input": 10, "output": 5 },
            "time": { "created": 1700000000000.0 }
        }"#;
        for row in ["first", "second"] {
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![row, "ses_1", data_json],
            )
            .unwrap();
        }
        drop(conn);

        let mut delivered = 0;
        let error = visit_micode_sqlite(&db_path, &mut |_| {
            delivered += 1;
            Err("sink rejected micode message".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "sink rejected micode message");
        assert_eq!(delivered, 1);
    }
}
