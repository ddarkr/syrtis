//! Goose session parser
//!
//! Parses session rows from Goose's SQLite sessions database:
//! - Primary: `~/.local/share/goose/sessions/sessions.db`
//! - macOS: `~/Library/Application Support/goose/sessions/sessions.db`
//! - Legacy Block/goose: `~/.local/share/Block/goose/sessions/sessions.db`
//! - Custom: `$GOOSE_PATH_ROOT/data/sessions/sessions.db`

use super::UnifiedMessage;
use crate::{provider_identity, TokenBreakdown};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;
use tracing::warn;

#[derive(Debug, Deserialize)]
struct GooseModelConfig {
    model_name: String,
}

fn parse_model_config(json: &str) -> Option<String> {
    let mut bytes = json.as_bytes().to_vec();
    let config: GooseModelConfig = simd_json::from_slice(&mut bytes).ok()?;
    let name = config.model_name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn timestamp_secs_to_ms(timestamp: f64) -> i64 {
    if timestamp > 1e12 {
        timestamp as i64
    } else {
        // Seconds -> milliseconds. Scale in f64 to keep sub-second precision,
        // then clamp into i64 range so a garbage/huge timestamp saturates
        // rather than producing an undefined cast during the conversion.
        let millis = timestamp * 1000.0;
        if millis.is_nan() {
            0
        } else {
            millis.clamp(i64::MIN as f64, i64::MAX as f64) as i64
        }
    }
}

fn resolved_provider(provider_name: Option<String>, model_id: &str) -> String {
    provider_name
        .filter(|p| !p.trim().is_empty())
        .and_then(|p| provider_identity::canonical_provider(p.trim()))
        .or_else(|| provider_identity::inferred_provider_from_model(model_id).map(str::to_string))
        .unwrap_or_else(|| "goose".to_string())
}

fn parse_created_at(s: &str) -> f64 {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_millis() as f64;
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return dt.and_utc().timestamp_millis() as f64;
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .unwrap_or_default()
            .and_utc()
            .timestamp_millis() as f64;
    }
    0.0
}

pub fn parse_goose_sqlite(db_path: &Path) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    // Preserve the legacy collector's good prefix on a SQLite step failure.
    // The visitor still returns Err and source observation still rejects the read.
    let _ = visit_goose_sqlite(db_path, &mut |message| {
        messages.push(message);
        Ok(())
    });
    messages
}

pub(crate) fn visit_goose_sqlite(
    db_path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let conn = match super::utils::observe_sqlite(Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )) {
        Ok(c) => c,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to open Goose sessions database"
            );
            return Err(err.to_string());
        }
    };

    let query = r#"
        SELECT
            id,
            model_config_json,
            provider_name,
            created_at,
            total_tokens,
            input_tokens,
            output_tokens,
            accumulated_total_tokens,
            accumulated_input_tokens,
            accumulated_output_tokens
        FROM sessions
        WHERE model_config_json IS NOT NULL
          AND TRIM(model_config_json) != ''
    "#;

    let mut stmt = match super::utils::prepare_source_sqlite(
        &conn,
        query,
        &[(
            "sessions",
            &[
                "id",
                "model_config_json",
                "provider_name",
                "created_at",
                "total_tokens",
                "input_tokens",
                "output_tokens",
                "accumulated_total_tokens",
                "accumulated_input_tokens",
                "accumulated_output_tokens",
            ],
        )],
    ) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to prepare Goose session query"
            );
            return Err(err.to_string());
        }
    };

    let rows = match super::utils::observe_sqlite(stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
        ))
    }))
    .map(super::utils::source_rows)
    {
        Ok(r) => r,
        Err(err) => {
            warn!(
                db_path = %db_path.display(),
                error = %err,
                "Failed to execute Goose session query"
            );
            return Err(err.to_string());
        }
    };

    for row in rows {
        let (
            session_id,
            model_config_json,
            provider_name,
            created_at,
            total_tokens,
            input_tokens,
            output_tokens,
            accumulated_total_tokens,
            accumulated_input_tokens,
            accumulated_output_tokens,
        ) = match row {
            Ok(row) => row,
            Err(err) => {
                warn!(
                    db_path = %db_path.display(),
                    error = %err,
                    "Failed to decode Goose session row"
                );
                if super::utils::sqlite_read_failed(&err) {
                    return Err(err.to_string());
                }
                continue;
            }
        };
        let Some(model_id) = model_config_json.as_deref().and_then(parse_model_config) else {
            continue;
        };

        let created_at_ts = parse_created_at(&created_at);

        let input = accumulated_input_tokens
            .or(input_tokens)
            .unwrap_or(0)
            .max(0);
        let output = accumulated_output_tokens
            .or(output_tokens)
            .unwrap_or(0)
            .max(0);
        let total = accumulated_total_tokens
            .or(total_tokens)
            .unwrap_or(0)
            .max(0);

        if input == 0 && output == 0 && total == 0 {
            continue;
        }

        let provider = resolved_provider(provider_name, &model_id);
        let mut msg = UnifiedMessage::new(
            "goose",
            model_id,
            provider,
            session_id.clone(),
            timestamp_secs_to_ms(created_at_ts),
            TokenBreakdown {
                input,
                output,
                cache_read: 0,
                cache_write: 0,
                // INFERRED, not a real field: Goose's schema has no reasoning
                // token column. We heuristically attribute any gap between the
                // reported total and (input + output) to reasoning. This is a
                // best-effort estimate, not a measured count.
                reasoning: if total > input + output {
                    (total - input - output).max(0)
                } else {
                    0
                },
                cache_write_1h: 0,
            },
            0.0,
        );
        msg.dedup_key = Some(session_id);
        sink(msg)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visitor_skips_invalid_rows_and_stops_on_sink_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE sessions (
                id TEXT, model_config_json TEXT, provider_name TEXT, created_at TEXT,
                total_tokens INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                accumulated_total_tokens INTEGER, accumulated_input_tokens INTEGER,
                accumulated_output_tokens INTEGER
            );
            INSERT INTO sessions VALUES
                ('first', '{"model_name":"gpt-5"}', NULL, '2026-04-14', 9, 2, 3, 20, 7, 8),
                ('bad-type', '{"model_name":"gpt-5"}', NULL, '2026-04-14', 9, 'bad', 3, NULL, NULL, NULL),
                ('bad-json', 'broken', NULL, '2026-04-14', 9, 2, 3, NULL, NULL, NULL),
                ('last', '{"model_name":"gpt-5"}', NULL, '2026-04-14', 9, 2, 3, NULL, NULL, NULL);"#,
        ).unwrap();
        let mut rows = Vec::new();
        visit_goose_sqlite(&path, &mut |message| {
            rows.push(message);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.session_id.as_str())
                .collect::<Vec<_>>(),
            ["first", "last"]
        );
        assert_eq!(rows[0].dedup_key.as_deref(), Some("first"));
        assert_eq!(rows[0].tokens.input, 7);
        assert_eq!(rows[0].tokens.output, 8);
        assert_eq!(rows[0].tokens.reasoning, 5);
        assert_eq!(rows[0].provider_id, "openai");
        let mut calls = 0;
        assert_eq!(
            visit_goose_sqlite(&path, &mut |_| {
                calls += 1;
                Err("sink stopped".to_string())
            }),
            Err("sink stopped".to_string())
        );
        assert_eq!(calls, 1);
        conn.execute_batch("DROP TABLE sessions;").unwrap();
        assert!(visit_goose_sqlite(&path, &mut |_| panic!("unexpected row")).is_err());
        assert!(parse_goose_sqlite(&path).is_empty());
    }

    #[test]
    fn step_failure_preserves_collector_prefix_but_fails_visitor_and_observation() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sessions.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE input (value INTEGER);
            INSERT INTO input VALUES (7), (-9223372036854775808), (9);
            CREATE VIEW sessions AS SELECT
                CAST(rowid AS TEXT) AS id,
                '{"model_name":"gpt-5"}' AS model_config_json,
                NULL AS provider_name, '2026-04-14' AS created_at,
                NULL AS total_tokens, abs(value) AS input_tokens,
                1 AS output_tokens, NULL AS accumulated_total_tokens,
                NULL AS accumulated_input_tokens, NULL AS accumulated_output_tokens
            FROM input;"#,
        )
        .unwrap();

        let legacy = parse_goose_sqlite(&path);
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].tokens.input, 7);
        assert!(
            super::super::utils::source_read_scope(&path, || parse_goose_sqlite(&path)).is_err()
        );

        let mut prefix = Vec::new();
        assert!(visit_goose_sqlite(&path, &mut |message| {
            prefix.push(message);
            Ok(())
        })
        .is_err());
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].tokens.input, 7);
    }

    #[test]
    fn test_parse_model_config_valid() {
        let json = r#"{"model_name":"claude-sonnet-4-20250514","context_limit":200000}"#;
        assert_eq!(
            parse_model_config(json),
            Some("claude-sonnet-4-20250514".to_string())
        );
    }

    #[test]
    fn test_parse_model_config_empty_name() {
        let json = r#"{"model_name":"  ","context_limit":200000}"#;
        assert_eq!(parse_model_config(json), None);
    }

    #[test]
    fn test_parse_model_config_invalid_json() {
        assert_eq!(parse_model_config("not json"), None);
    }

    #[test]
    fn test_timestamp_secs_to_ms() {
        assert_eq!(timestamp_secs_to_ms(1_700_000_000.0), 1_700_000_000_000);
        assert_eq!(timestamp_secs_to_ms(1_700_000_000_000.0), 1_700_000_000_000);
    }

    #[test]
    fn test_parse_created_at_rfc3339() {
        let ts = parse_created_at("2026-04-14T16:18:53Z");
        assert!(ts > 0.0);
    }

    #[test]
    fn test_parse_created_at_sqlite_timestamp() {
        let ts = parse_created_at("2026-04-14 16:18:53");
        assert!(ts > 0.0);
        let expected =
            chrono::NaiveDateTime::parse_from_str("2026-04-14 16:18:53", "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_utc()
                .timestamp_millis() as f64;
        assert_eq!(ts, expected);
    }

    #[test]
    fn test_parse_created_at_date_only() {
        let ts = parse_created_at("2026-04-14");
        assert!(ts > 0.0);
    }

    #[test]
    fn test_parse_created_at_invalid() {
        assert_eq!(parse_created_at("not a date"), 0.0);
    }
}
