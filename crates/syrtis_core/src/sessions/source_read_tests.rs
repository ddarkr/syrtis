use super::*;
use crate::sessions::{self, UnifiedMessage};
use std::io::{BufReader, Cursor};
use tempfile::TempDir;

type Parser = fn(&Path) -> Vec<UnifiedMessage>;

// Include format-specific dispatch, and wrappers sharing Pi/Roo implementations.
const FILE_PARSERS: &[(&str, Parser)] = &[
    ("amp.json", sessions::amp::parse_amp_file),
    (
        "antigravity.jsonl",
        sessions::antigravity::parse_antigravity_file,
    ),
    ("augment.json", sessions::augment::parse_augment_file),
    ("cline.json", sessions::cline::parse_cline_file),
    ("codebuff.json", sessions::codebuff::parse_codebuff_file),
    ("copilot.jsonl", sessions::copilot::parse_copilot_file),
    ("cursor.csv", sessions::cursor::parse_cursor_file),
    ("usage.json", sessions::cursor::parse_cursor_file),
    ("droid.settings.json", sessions::droid::parse_droid_file),
    ("session-gemini.json", sessions::gemini::parse_gemini_file),
    ("gemini.jsonl", sessions::gemini::parse_gemini_file),
    ("gjc.jsonl", sessions::gjc::parse_gjc_file),
    ("updates.jsonl", sessions::grok::parse_grok_updates_file),
    ("unified.jsonl", sessions::grok::parse_grok_unified_log_file),
    ("hindsight.jsonl", sessions::hindsight::parse_hindsight_file),
    ("jcode.json", sessions::jcode::parse_jcode_file),
    ("junie.jsonl", sessions::junie::parse_junie_file),
    ("kilocode.json", sessions::kilocode::parse_kilocode_file),
    ("kimchi.jsonl", sessions::kimchi::parse_kimchi_file),
    ("kimi.jsonl", sessions::kimi::parse_kimi_file),
    ("kimi-code.jsonl", sessions::kimi::parse_kimi_code_file),
    ("kiro.json", sessions::kiro::parse_kiro_file),
    ("sess_example/session.json", sessions::kiro::parse_kiro_file),
    ("kiro.chat", sessions::kiro::parse_kiro_file),
    ("muse.jsonl", sessions::muse::parse_muse_file),
    ("mux.json", sessions::mux::parse_mux_file),
    ("omp.jsonl", sessions::omp::parse_omp_file),
    ("sessions.json", sessions::openclaw::parse_openclaw_index),
    (
        "openclaw.jsonl",
        sessions::openclaw::parse_openclaw_transcript,
    ),
    (
        "openclaw.jsonl.zst",
        sessions::openclaw::parse_openclaw_transcript,
    ),
    (
        "opencodereview.jsonl",
        sessions::opencodereview::parse_opencodereview_file,
    ),
    ("pi.jsonl", sessions::pi::parse_pi_file),
    ("qwen.jsonl", sessions::qwen::parse_qwen_file),
    ("reasonix.jsonl", sessions::reasonix::parse_reasonix_file),
    ("roocode.json", sessions::roocode::parse_roocode_file),
    ("senpi.jsonl", sessions::senpi::parse_senpi_file),
    ("trae.json", |path| {
        sessions::trae::parse_trae_file("trae", path)
    }),
    ("warp.json", sessions::warp::parse_warp_file),
];

const SQLITE_PARSERS: &[(&str, Parser)] = &[
    (
        "antigravity-cli",
        sessions::antigravity_cli::parse_antigravity_cli_file,
    ),
    (
        "copilot-desktop",
        sessions::copilot_desktop::parse_copilot_desktop_db,
    ),
    ("crush", sessions::crush::parse_crush_sqlite),
    ("goose", sessions::goose::parse_goose_sqlite),
    ("hermes", sessions::hermes::parse_hermes_sqlite),
    ("kilo", sessions::kilo::parse_kilo_sqlite),
    ("kiro", sessions::kiro::parse_kiro_sqlite),
    ("micode", sessions::micode::parse_micode_sqlite),
    ("octofriend", sessions::synthetic::parse_octofriend_sqlite),
    ("zcode", sessions::zcode::parse_zcode_sqlite),
    ("zed", sessions::zed::parse_zed_sqlite),
];

#[test]
fn every_cold_parser_observes_required_source_open_failure() {
    let directory = TempDir::new().unwrap();
    for (name, parse) in FILE_PARSERS.iter().chain(SQLITE_PARSERS) {
        let path = directory.path().join(name);
        assert!(parse(&path).is_empty(), "legacy {name}");
        assert_eq!(
            source_read_scope(&path, || parse(&path)).unwrap_err(),
            "session source read incomplete",
            "guarded {name}",
        );
    }
}

#[test]
fn every_sqlite_parser_distinguishes_unsupported_schema_from_corruption() {
    let directory = TempDir::new().unwrap();
    let unsupported = directory.path().join("unsupported.db");
    let connection = Connection::open(&unsupported).unwrap();
    connection
        .execute_batch("CREATE TABLE unrelated (id INTEGER)")
        .unwrap();
    drop(connection);
    let corrupt = directory.path().join("corrupt.db");
    std::fs::write(&corrupt, b"not a SQLite database").unwrap();
    for (name, parse) in SQLITE_PARSERS {
        assert!(
            source_read_scope(&unsupported, || parse(&unsupported))
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert!(parse(&corrupt).is_empty(), "legacy {name}");
        assert!(
            source_read_scope(&corrupt, || parse(&corrupt)).is_err(),
            "guarded {name}"
        );
    }
}

struct FailsAfterPrefix(Cursor<&'static [u8]>);

impl Read for FailsAfterPrefix {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.0.position() == self.0.get_ref().len() as u64 {
            Err(io::Error::other("private mount read failure"))
        } else {
            self.0.read(buffer)
        }
    }
}

#[test]
fn hard_io_after_prefix_rejects_scope_and_terminates_legacy_continue_loop() {
    let path = Path::new("private-session.jsonl");
    let mut prefix = Vec::new();
    let result = source_read_scope(path, || {
        let reader = SourceReader(FailsAfterPrefix(Cursor::new(b"first\nsecond\n")), false);
        // The existing parsers continue on line errors; exercise that exact
        // idiom rather than manufacturing an outcome flag in this test.
        for line in BufReader::new(reader).lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            prefix.push(line);
        }
    });
    assert_eq!(prefix, ["first", "second"]);
    assert_eq!(result.unwrap_err(), "session source read incomplete");
    assert!(source_read_scope(path, || lossy_lines(&b"good\n"[..]).collect::<Vec<_>>()).is_ok());

    let mut legacy = SourceReader(FailsAfterPrefix(Cursor::new(b"")), false);
    assert!(legacy.read(&mut [0]).is_err());
    assert!(legacy.read(&mut [0]).is_err());

    assert!(source_read_scope(path, || {
        assert_eq!(
            lossy_lines(BufReader::new(FailsAfterPrefix(Cursor::new(b"valid\n"))))
                .next()
                .unwrap(),
            "valid"
        );
        // Exhaust a fresh stream: an error hidden by LossyLines still rejects.
        lossy_lines(BufReader::new(FailsAfterPrefix(Cursor::new(b"valid\n")))).collect::<Vec<_>>()
    })
    .is_err());
}

#[test]
fn interrupted_read_retries_and_invalid_utf8_keeps_original_semantics() {
    struct InterruptedOnce(bool);
    impl Read for InterruptedOnce {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, false) {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Cursor::new(b"").read(buffer)
            }
        }
    }
    let path = Path::new("session.jsonl");
    source_read_scope(path, || {
        let mut reader = SourceReader(InterruptedOnce(true), false);
        reader.read_to_end(&mut Vec::new()).unwrap();
        let mut lines = Vec::new();
        for line in BufReader::new(SourceReader(Cursor::new(b"a\n\xff\nb\n"), false)).lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => continue,
            };
            lines.push(line);
        }
        assert_eq!(lines, ["a", "b"]);
    })
    .unwrap();
}

#[test]
fn optional_missing_and_malformed_sources_are_not_io_failure() {
    let directory = TempDir::new().unwrap();
    let missing = directory.path().join("optional.json");
    source_read_scope(&missing, || {
        assert!(open_optional_source_file(&missing).is_err());
        assert!(read_optional_file_or_none(&missing).is_none());
        assert!(read_optional_source_text(&missing).is_err());
        assert!(!optional_source_is_file(&missing));
        assert!(!optional_source_exists(&missing));
    })
    .unwrap();

    let path = directory.path().join("reasonix.jsonl");
    std::fs::write(&path, b"{\"ts\":1700000000000,\"model\":\"m\",\"prompt\":10,\"completion\":2,\"total\":12}\ninvalid \xff JSON\n{\"ts\":1700000001000,\"model\":\"m\",\"prompt\":20,\"completion\":3,\"total\":23}\n").unwrap();
    let messages =
        source_read_scope(&path, || sessions::reasonix::parse_reasonix_file(&path)).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].tokens.input, 10);
    assert_eq!(messages[1].tokens.input, 20);

    // This actual parser probes three optional siblings; absence is expected.
    let updates = directory.path().join("updates.jsonl");
    std::fs::write(&updates, b"invalid JSON\n").unwrap();
    assert!(
        source_read_scope(&updates, || sessions::grok::parse_grok_updates_file(
            &updates
        ))
        .unwrap()
        .is_empty()
    );
}

#[test]
fn sqlite_step_failure_after_prefix_is_not_a_successful_partial_generation() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE input (value INTEGER); INSERT INTO input VALUES (7), (-9223372036854775808), (9);").unwrap();
    let mut prefix = Vec::new();
    let outcome = source_read_scope(Path::new("source.db"), || {
        let mut statement = prepare_source_sqlite(
            &connection,
            "SELECT abs(value) FROM input",
            &[("input", &["value"])],
        )
        .unwrap();
        let rows = observe_sqlite(statement.query_map([], |row| row.get::<_, i64>(0))).unwrap();
        prefix.extend(source_rows(rows).flatten());
    });
    assert_eq!(prefix, [7]);
    assert!(outcome.is_err());
}

#[test]
fn sqlite_row_conversion_skip_is_complete_but_json_step_termination_is_not() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE input (value); INSERT INTO input VALUES (7), ('invalid integer'), (9);",
        )
        .unwrap();
    let values = source_read_scope(Path::new("source.db"), || {
        let mut statement = prepare_source_sqlite(
            &connection,
            "SELECT value FROM input",
            &[("input", &["value"])],
        )
        .unwrap();
        let rows = observe_sqlite(statement.query_map([], |row| row.get::<_, i64>(0))).unwrap();
        source_rows(rows).flatten().collect::<Vec<_>>()
    })
    .unwrap();
    assert_eq!(values, [7, 9]);

    connection.execute_batch("DELETE FROM input; INSERT INTO input VALUES ('{\"tokens\":7}'), ('bad JSON'), ('{\"tokens\":9}');").unwrap();
    let mut prefix = Vec::new();
    let outcome = source_read_scope(Path::new("source.db"), || {
        let mut statement = prepare_source_sqlite(
            &connection,
            "SELECT json_extract(value, '$.tokens') FROM input",
            &[("input", &["value"])],
        )
        .unwrap();
        let rows = observe_sqlite(statement.query_map([], |row| row.get::<_, i64>(0))).unwrap();
        prefix.extend(source_rows(rows).flatten());
    });
    assert_eq!(prefix, [7]);
    assert!(outcome.is_err());
}

#[test]
fn actual_goose_parser_rejects_step_failure_but_preserves_legacy_prefix() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("goose.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"
        CREATE TABLE input (value INTEGER);
        INSERT INTO input VALUES (7), (-9223372036854775808), (9);
        CREATE VIEW sessions AS SELECT
            CAST(rowid AS TEXT) AS id,
            '{"model_name":"m"}' AS model_config_json,
            'provider' AS provider_name,
            '2026-08-04T09:10:11Z' AS created_at,
            NULL AS total_tokens, abs(value) AS input_tokens,
            1 AS output_tokens, NULL AS accumulated_total_tokens,
            NULL AS accumulated_input_tokens, NULL AS accumulated_output_tokens
        FROM input;
    "#,
        )
        .unwrap();
    drop(connection);
    let legacy = sessions::goose::parse_goose_sqlite(&path);
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].tokens.input, 7);
    assert!(source_read_scope(&path, || sessions::goose::parse_goose_sqlite(&path)).is_err());
}

#[test]
fn schema_fallback_requires_verified_absence_not_arbitrary_prepare_error() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE current (id INTEGER)")
        .unwrap();
    let path = Path::new("source.db");
    source_read_scope(path, || {
        assert!(prepare_source_sqlite(
            &connection,
            "SELECT value FROM absent",
            &[("absent", &["value"])]
        )
        .is_err());
        assert!(prepare_source_sqlite(
            &connection,
            "SELECT old_column FROM current",
            &[("current", &["old_column"])]
        )
        .is_err());
    })
    .unwrap();
    assert!(source_read_scope(path, || {
        // An absent optional schema is not license to suppress a syntax error.
        assert!(prepare_source_sqlite(
            &connection,
            "SELECT FROM current",
            &[("absent", &["value"])]
        )
        .is_err());
    })
    .is_err());
    assert!(source_read_scope(path, || {
        assert!(prepare_source_sqlite(
            &connection,
            "SELECT unknown_function(id) FROM current",
            &[("current", &["id"])]
        )
        .is_err());
    })
    .is_err());
}

#[test]
fn nested_and_panicking_scopes_restore_the_enclosing_state() {
    let directory = TempDir::new().unwrap();
    let missing = directory.path().join("missing");
    source_read_scope(&missing, || {
        assert!(source_read_scope(&missing, || read_file_or_none(&missing)).is_err());
        assert!(std::panic::catch_unwind(|| source_read_scope(&missing, || {
            let _ = read_file_or_none(&missing);
            panic!("parser panic");
        }))
        .is_err());
    })
    .unwrap();
    assert!(source_read_scope(&missing, || {
        let _ = read_file_or_none(&missing);
        assert!(source_read_scope(&missing, || 42).is_ok());
    })
    .is_err());
    assert_eq!(SOURCE_READ_FAILED.get(), None);
}

#[test]
fn concurrent_workers_keep_read_outcomes_local_and_clear_between_sources() {
    let directory = TempDir::new().unwrap();
    let missing = directory.path().join("missing");
    let valid = directory.path().join("valid");
    std::fs::write(&valid, b"content").unwrap();
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|threads| {
        for worker in 0..4 {
            let (barrier, missing, valid) = (&barrier, &missing, &valid);
            threads.spawn(move || {
                for _ in 0..8 {
                    let path = if worker % 2 == 0 { missing } else { valid };
                    let outcome = source_read_scope(path, || {
                        let bytes = read_file_or_none(path);
                        barrier.wait();
                        bytes
                    });
                    assert_eq!(outcome.is_err(), worker % 2 == 0);
                    assert!(source_read_scope(valid, || read_file_or_none(valid)).is_ok());
                }
            });
        }
    });
    assert_eq!(SOURCE_READ_FAILED.get(), None);
}

#[test]
fn desktop_optional_events_absence_succeeds_but_real_read_failure_aborts() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("copilot.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            r#"
        CREATE TABLE sessions (
            id TEXT, model TEXT, total_input_tokens INTEGER,
            total_output_tokens INTEGER, total_cached_tokens INTEGER,
            total_reasoning_tokens INTEGER, created_at TEXT, agent TEXT
        );
        INSERT INTO sessions VALUES
            ('session', 'model', 7, 2, 0, 0, '2026-08-04T09:10:11Z', NULL);
    "#,
        )
        .unwrap();
    drop(connection);
    let messages = source_read_scope(&path, || {
        sessions::copilot_desktop::parse_copilot_desktop_db(&path)
    })
    .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tokens.input, 7);

    // Opening a directory succeeds on Unix, but the first real read fails.
    // On other targets the open itself fails: both are genuine hard IO errors.
    let events = directory.path().join("session-state/session/events.jsonl");
    std::fs::create_dir_all(&events).unwrap();
    assert_eq!(
        sessions::copilot_desktop::parse_copilot_desktop_db(&path).len(),
        1
    );
    assert!(source_read_scope(&path, || {
        sessions::copilot_desktop::parse_copilot_desktop_db(&path)
    })
    .is_err());
}

#[test]
fn offset_sql_input_errors_are_observed_without_breaking_missing_column_fallback() {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE current (id INTEGER)")
        .unwrap();
    let path = Path::new("source.db");
    let syntax_error = connection.prepare("SELECT FROM current").err().unwrap();
    // Bundled modern SQLite exposes preparation offsets in a distinct variant.
    // Exercise the real variant rather than constructing an error in the test.
    assert!(matches!(
        &syntax_error,
        rusqlite::Error::SqlInputError { .. }
    ));
    assert!(source_read_scope(path, || observe_sqlite::<()>(Err(syntax_error))).is_err());
    assert!(source_read_scope(path, || {
        prepare_source_sqlite(&connection, "SELECT FROM current", &[("current", &["id"])]).is_err()
    })
    .is_err());

    source_read_scope(path, || {
        assert!(prepare_source_sqlite(
            &connection,
            "SELECT old_column FROM current",
            &[("current", &["old_column"])]
        )
        .is_err());
    })
    .unwrap();
}
