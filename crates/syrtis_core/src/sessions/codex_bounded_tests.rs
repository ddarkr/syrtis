use super::*;
use serde_json::json;
use std::io::{Cursor, Write};
use tempfile::NamedTempFile;

fn line(file: &mut NamedTempFile, value: Value) {
    serde_json::to_writer(&mut *file, &value).unwrap();
    file.write_all(b"\n").unwrap();
    file.flush().unwrap();
}

fn token(total: i64, timestamp: Option<&str>) -> Value {
    let mut value = json!({"type":"event_msg","payload":{"type":"token_count","info":{
        "total_token_usage":{"input_tokens":total,"cached_input_tokens":0,"output_tokens":total},
        "last_token_usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}
    }}});
    if let Some(timestamp) = timestamp {
        value["timestamp"] = json!(timestamp);
    }
    value
}

fn collect(
    path: &Path,
    offset: u64,
    state: CodexParseState,
) -> (Vec<(UnifiedMessage, bool)>, CodexVisitResult) {
    let mut rows = Vec::new();
    let outcome = visit_codex_file_incremental(
        path,
        offset,
        state,
        &mut CodexForkTurnIds::default(),
        &mut |message: &UnifiedMessage, fallback| {
            rows.push((message.clone(), fallback));
            Ok(())
        },
    )
    .unwrap();
    (rows, outcome)
}

#[test]
fn bounded_full_and_resume_preserve_rows_state_flags_and_duration() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"id":"stable","source":"exec","model_provider":"azure","agent_nickname":"worker","cwd":"/project/work"}}),
    );
    line(
        &mut file,
        json!({"type":"turn_context","timestamp":"2026-01-01T00:00:00Z","payload":{"model":"gpt-5.5"}}),
    );
    line(
        &mut file,
        json!({"type":"event_msg","timestamp":"2026-01-01T00:00:01Z","payload":{"type":"user_message","message":"hello"}}),
    );
    line(&mut file, token(1, Some("2026-01-01T00:00:03Z")));
    let (mut rows, initial) = collect(file.path(), 0, CodexParseState::default());
    assert_eq!(rows[0].0.duration_ms, Some(2000));
    assert!(rows[0].0.is_turn_start);
    assert_eq!(rows[0].0.provider_id, "azure");
    assert_eq!(rows[0].0.agent.as_deref(), Some("headless"));
    let prefix = crate::message_cache::build_codex_incremental_cache(
        file.path(),
        initial.consumed_offset,
        initial.state.clone(),
    )
    .unwrap();
    line(&mut file, token(2, None));
    line(&mut file, token(3, Some("2026-01-01T00:00:06Z")));
    assert!(crate::message_cache::codex_prefix_matches(
        file.path(),
        &prefix
    ));
    let (appended, resumed) = collect(file.path(), initial.consumed_offset, initial.state);
    rows.extend(appended);
    let (full, completed) = collect(file.path(), 0, CodexParseState::default());
    assert_eq!(rows, full);
    assert_eq!(
        rows.iter()
            .map(|(_, fallback)| *fallback)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    assert_eq!(resumed.consumed_offset, completed.consumed_offset);
    assert_eq!(
        serde_json::to_value(resumed.state).unwrap(),
        serde_json::to_value(completed.state).unwrap()
    );
    assert!(resumed.parse_succeeded && completed.parse_succeeded);
    assert!(!resumed.unresolved_model_events && !completed.unresolved_model_events);
    let materialized = parse_codex_file_incremental(file.path(), 0, CodexParseState::default());
    assert_eq!(
        materialized.messages,
        full.iter()
            .map(|(message, _)| message.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(materialized.fallback_timestamp_indices, [1]);
}

#[test]
fn bounded_late_model_missing_times_and_unknown_boundaries_preserve_order() {
    let mut file = NamedTempFile::new().unwrap();
    line(&mut file, token(1, None));
    line(&mut file, token(2, Some("2026-01-01T00:00:01Z")));
    line(&mut file, json!({"model":"gpt-5.5","type":"metadata"}));
    line(&mut file, token(3, Some("2026-01-01T00:00:02Z")));
    let (rows, outcome) = collect(file.path(), 0, CodexParseState::default());
    assert!(outcome.parse_succeeded);
    assert!(!outcome.unresolved_model_events);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|(message, _)| message.model_id.as_str())
            .collect::<Vec<_>>(),
        ["gpt-5.5"; 3]
    );
    assert_eq!(
        rows.iter()
            .map(|(_, fallback)| *fallback)
            .collect::<Vec<_>>(),
        [true, false, false]
    );
    assert_eq!(rows[0].0.timestamp, file_modified_timestamp_ms(file.path()));
    // Existing pending dedup keys retain the unknown-model key minted before
    // model repair; do not silently "fix" that separate legacy behavior.
    assert!(rows[0]
        .0
        .dedup_key
        .as_deref()
        .unwrap()
        .contains(":unknown:"));
    assert_eq!(
        parse_codex_file(file.path()),
        rows.iter()
            .map(|(message, _)| message.clone())
            .collect::<Vec<_>>()
    );

    let mut boundary = NamedTempFile::new().unwrap();
    line(&mut boundary, token(1, None));
    line(
        &mut boundary,
        json!({"type":"event_msg","payload":{"type":"user_message","message":"new turn"}}),
    );
    line(
        &mut boundary,
        json!({"type":"turn_context","payload":{"model":"gpt-5.5"}}),
    );
    line(&mut boundary, token(2, None));
    let (rows, outcome) = collect(boundary.path(), 0, CodexParseState::default());
    assert!(outcome.unresolved_model_events);
    assert_eq!(rows[0].0.model_id, "unknown");
    assert_eq!(rows[1].0.model_id, "gpt-5.5");
    assert!(rows[1].0.is_turn_start);
}

#[test]
fn bounded_fork_resume_skips_replay_and_keeps_cumulative_delta() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"id":"child","forked_from_id":"parent","model_provider":"azure","cwd":"/repo-child"}}),
    );
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"id":"parent"}}),
    );
    line(&mut file, token(10, Some("2026-01-01T00:00:00Z")));
    let (rows, prefix) = collect(file.path(), 0, CodexParseState::default());
    assert!(rows.is_empty());
    assert!(prefix.state.forked_child_waiting_for_turn_context);
    line(
        &mut file,
        json!({"type":"turn_context","timestamp":"2026-01-01T00:00:01Z","payload":{"model":"gpt-5.5"}}),
    );
    line(&mut file, token(10, Some("2026-01-01T00:00:02Z")));
    line(
        &mut file,
        json!({"type":"event_msg","timestamp":"2026-01-01T00:00:03Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12,"output_tokens":12}}}}),
    );
    let (resumed, outcome) = collect(file.path(), prefix.consumed_offset, prefix.state);
    let (full, completed) = collect(file.path(), 0, CodexParseState::default());
    assert_eq!(resumed, full);
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].0.tokens.input, 2);
    assert_eq!(full[0].0.tokens.output, 2);
    assert_eq!(full[0].0.provider_id, "azure");
    assert!(full[0]
        .0
        .dedup_key
        .as_deref()
        .unwrap()
        .contains("parent:azure:gpt-5.5"));
    assert_eq!(
        serde_json::to_value(outcome.state).unwrap(),
        serde_json::to_value(completed.state).unwrap()
    );
}

#[test]
fn bounded_partial_lines_rewrites_and_truncations_keep_resume_admission_exact() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"model":"gpt-5.5"}}),
    );
    line(&mut file, token(1, None));
    let (_, complete) = collect(file.path(), 0, CodexParseState::default());
    let prefix = crate::message_cache::build_codex_incremental_cache(
        file.path(),
        complete.consumed_offset,
        complete.state,
    )
    .unwrap();
    file.write_all(b"{\"type\":").unwrap();
    file.flush().unwrap();
    let (_, partial) = collect(file.path(), 0, CodexParseState::default());
    assert!(!partial.parse_succeeded);
    assert_eq!(
        partial.consumed_offset,
        file.as_file().metadata().unwrap().len()
    );
    assert!(crate::message_cache::build_codex_incremental_cache(
        file.path(),
        partial.consumed_offset,
        partial.state
    )
    .is_none());
    assert!(crate::message_cache::codex_prefix_matches(
        file.path(),
        &prefix
    ));
    file.as_file_mut().seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b" ").unwrap();
    file.flush().unwrap();
    assert!(!crate::message_cache::codex_prefix_matches(
        file.path(),
        &prefix
    ));
    file.as_file().set_len(0).unwrap();
    assert!(!crate::message_cache::codex_prefix_matches(
        file.path(),
        &prefix
    ));
    assert!(visit_codex_file_incremental(
        file.path(),
        prefix.consumed_offset,
        prefix.state,
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| Ok(())
    )
    .is_err());

    // A valid final JSON record without a newline is parsed, but never resumable.
    let mut final_record = NamedTempFile::new().unwrap();
    serde_json::to_writer(&mut final_record, &token(1, None)).unwrap();
    final_record.flush().unwrap();
    let (rows, outcome) = collect(final_record.path(), 0, CodexParseState::default());
    assert!(outcome.parse_succeeded && outcome.unresolved_model_events);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        outcome.consumed_offset,
        final_record.as_file().metadata().unwrap().len()
    );
    assert!(crate::message_cache::build_codex_incremental_cache(
        final_record.path(),
        outcome.consumed_offset,
        outcome.state
    )
    .is_none());
}

#[test]
fn bounded_io_and_sink_failures_abort_complete_publication() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"model":"gpt-5.5"}}),
    );
    line(&mut file, token(1, None));
    let error = visit_codex_file_incremental(
        file.path(),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| Err("sink failed".into()),
    )
    .unwrap_err();
    assert_eq!(error, "sink failed");
    file.write_all(&[0xff, b'\n']).unwrap();
    file.flush().unwrap();
    let mut delivered = 0;
    let error = visit_codex_file_incremental(
        file.path(),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| {
            delivered += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.contains("reading Codex source"));
    assert_eq!(delivered, 1);
    let legacy = parse_codex_file_incremental(file.path(), 0, CodexParseState::default());
    assert!(!legacy.parse_succeeded);
    assert_eq!(legacy.messages.len(), 1);
    let missing = tempfile::TempDir::new().unwrap();
    assert!(visit_codex_file_incremental(
        &missing.path().join("absent.jsonl"),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| Ok(())
    )
    .is_err());
}

#[test]
fn pending_rows_use_an_actual_disk_file_and_stream_in_order() {
    let mut pending = PendingCodexMessages::default();
    let mut message = UnifiedMessage::new_with_agent(
        "codex",
        "unknown",
        "openai",
        "s".repeat(4096),
        0,
        TokenBreakdown::default(),
        0.0,
        None,
    );
    for ordinal in 0..2048 {
        message.timestamp = ordinal;
        pending.push(&message, ordinal % 2 == 0).unwrap();
    }
    let database = pending.database.as_ref().unwrap();
    assert!(
        temp_file_bytes(database) > 2 * 1024 * 1024,
        "TEMP rows must spill beyond the capped pager"
    );
    let mut count = 0;
    pending
        .flush(
            &mut |row: &UnifiedMessage, fallback| {
                assert_eq!(row.timestamp, count);
                assert_eq!(fallback, count % 2 == 0);
                assert_eq!(row.model_id, "gpt-5.5");
                count += 1;
                Ok(())
            },
            "gpt-5.5",
        )
        .unwrap();
    assert_eq!(count, 2048);
    assert!(pending.is_empty());
    pending.push(&message, true).unwrap();
    assert_eq!(
        pending
            .flush(
                &mut |_: &UnifiedMessage, _| Err("pending sink failed".into()),
                "gpt-5.5"
            )
            .unwrap_err(),
        "pending sink failed"
    );
    assert!(!pending.is_empty());
}

#[test]
fn large_model_late_source_streams_without_reordering_or_dropping_rows() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"cwd":format!("/repo/{}", "x".repeat(4096))}}),
    );
    for total in 1..=2048 {
        line(&mut file, token(total, None));
    }
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"model":"gpt-5.5"}}),
    );
    let mut count = 0;
    let outcome = visit_codex_file_incremental(
        file.path(),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |row: &UnifiedMessage, fallback| {
            count += 1;
            assert!(fallback);
            assert_eq!(row.model_id, "gpt-5.5");
            assert!(row
                .dedup_key
                .as_deref()
                .unwrap()
                .ends_with(&format!(":{count}:{count}:0:0")));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 2048);
    assert!(outcome.parse_succeeded && !outcome.unresolved_model_events);
    assert_eq!(
        outcome.consumed_offset,
        file.as_file().metadata().unwrap().len()
    );

    // Oversized individual compact/source records are accepted, not dropped.
    let input = format!(
        "{}\n{}\n",
        json!({"type":"session_meta","payload":{"agent_nickname":"x".repeat(9 * 1024 * 1024)}}),
        token(1, None)
    );
    let mut count = 0;
    let outcome = visit_codex_reader(
        Cursor::new(input),
        "oversized",
        0,
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |row: &UnifiedMessage, _| {
            assert_eq!(row.agent.as_ref().unwrap().len(), 9 * 1024 * 1024);
            count += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 1);
    assert!(outcome.parse_succeeded && outcome.unresolved_model_events);
}

fn temp_file_bytes(database: &rusqlite::Connection) -> i64 {
    let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: SQLite owns the TEMP file for this live connection. FILE_POINTER
    // writes one sqlite3_file pointer, used only while the connection is alive.
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            database.handle(),
            c"temp".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
    assert!(!file.is_null());
    // SAFETY: FILE_POINTER returned a non-null file owned by this connection.
    assert!(!unsafe { (*file).pMethods }.is_null());
    let mut bytes = 0;
    // SAFETY: A successful FILE_POINTER result belongs to the active SQLite
    // VFS; xFileSize is invoked with its matching live file and output pointer.
    let result = unsafe { ((*(*file).pMethods).xFileSize.unwrap())(file, &mut bytes) };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK);
    bytes
}

struct ScratchFailureGuard(bool);

impl ScratchFailureGuard {
    fn enable() -> Self {
        Self(FAIL_CODEX_SCRATCH.replace(true))
    }
}

impl Drop for ScratchFailureGuard {
    fn drop(&mut self) {
        FAIL_CODEX_SCRATCH.set(self.0);
    }
}

#[test]
fn bounded_scratch_failure_is_explicit_but_materialized_read_is_independent() {
    let mut file = NamedTempFile::new().unwrap();
    line(&mut file, token(1, None));
    line(&mut file, token(2, None));
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"model":"gpt-5.5"}}),
    );
    let _failure = ScratchFailureGuard::enable();
    let legacy = parse_codex_file_incremental(file.path(), 0, CodexParseState::default());
    assert!(legacy.parse_succeeded && !legacy.unresolved_model_events);
    assert_eq!(legacy.messages.len(), 2);
    assert_eq!(parse_codex_file(file.path()), legacy.messages);
    let mut delivered = 0;
    let error = visit_codex_file_incremental(
        file.path(),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| {
            delivered += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(error, "injected Codex scratch failure");
    assert_eq!(delivered, 0);

    let mut fork = NamedTempFile::new().unwrap();
    line(
        &mut fork,
        json!({"type":"session_meta","payload":{"id":"child","forked_from_id":"parent"}}),
    );
    line(
        &mut fork,
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"announced"}}),
    );
    let legacy = parse_codex_file_incremental(fork.path(), 0, CodexParseState::default());
    assert!(legacy.parse_succeeded);
    assert!(legacy
        .state
        .forked_child_task_started_turn_ids
        .contains("announced"));
    assert!(visit_codex_file_incremental(
        fork.path(),
        0,
        CodexParseState::default(),
        &mut CodexForkTurnIds::default(),
        &mut |_: &UnifiedMessage, _| Ok(()),
    )
    .unwrap_err()
    .contains("scratch failure"));
}

#[test]
fn fork_task_history_spills_and_resumes_with_exact_membership() {
    let mut file = NamedTempFile::new().unwrap();
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"id":"00000000-03e8-7000-8000-000000000000","forked_from_id":"parent"}}),
    );
    line(
        &mut file,
        json!({"type":"session_meta","payload":{"id":"parent"}}),
    );
    for ordinal in 0..2048 {
        line(
            &mut file,
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":format!("task-{ordinal}-{}", "x".repeat(4096)), "started_at":2}}),
        );
    }
    let mut membership = CodexForkTurnIds::default();
    let prefix = visit_codex_file_incremental(
        file.path(),
        0,
        CodexParseState::default(),
        &mut membership,
        &mut |_: &UnifiedMessage, _| panic!("replay prefix must not emit rows"),
    )
    .unwrap();
    assert!(prefix.parse_succeeded && !prefix.unresolved_model_events);
    assert!(prefix.state.forked_child_task_started_turn_ids.is_empty());
    assert!(temp_file_bytes(membership.database.as_ref().unwrap()) > 2 * 1024 * 1024);
    let legacy = parse_codex_file_incremental(file.path(), 0, CodexParseState::default());
    assert_eq!(legacy.state.forked_child_task_started_turn_ids.len(), 2048);
    let mut restored = CodexForkTurnIds::default();
    let mut count = 0;
    membership
        .visit(&mut |id| {
            assert!(legacy.state.forked_child_task_started_turn_ids.contains(id));
            restored.insert(id)?;
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 2048);
    assert!(!restored.contains("not-announced").unwrap());
    // A legacy (non-v7) turn cannot leave replay without its task-start id.
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"turn_id":"not-announced","model":"wrong-replay-model"}}),
    );
    line(&mut file, token(10, None));
    line(
        &mut file,
        json!({"type":"turn_context","payload":{"turn_id":format!("task-2047-{}", "x".repeat(4096)),"model":"gpt-5.5"}}),
    );
    line(&mut file, token(10, None));
    line(&mut file, token(11, None));
    let mut resumed_rows = Vec::new();
    let resumed = visit_codex_file_incremental(
        file.path(),
        prefix.consumed_offset,
        prefix.state,
        &mut restored,
        &mut |row: &UnifiedMessage, fallback| {
            resumed_rows.push((row.clone(), fallback));
            Ok(())
        },
    )
    .unwrap();
    let (full_rows, full) = collect(file.path(), 0, CodexParseState::default());
    assert_eq!(resumed_rows, full_rows);
    assert_eq!(resumed_rows.len(), 1);
    assert_eq!(resumed_rows[0].0.model_id, "gpt-5.5");
    assert_eq!(resumed_rows[0].0.tokens.input, 1);
    assert_eq!(
        serde_json::to_value(resumed.state).unwrap(),
        serde_json::to_value(full.state).unwrap()
    );
    let mut count = 0;
    restored
        .visit(&mut |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 0);
}
