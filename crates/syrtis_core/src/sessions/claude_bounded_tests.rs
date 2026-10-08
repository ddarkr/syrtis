use super::*;
use std::io::{self, Cursor, Read, Write};

fn write_rows(path: &Path, rows: &[Value]) {
    let mut file = std::fs::File::create(path).unwrap();
    for row in rows {
        serde_json::to_writer(&mut file, row).unwrap();
        writeln!(file).unwrap();
    }
}

fn assistant(id: &str, input: i64) -> Value {
    serde_json::json!({"type":"assistant", "timestamp":"2024-12-01T10:00:01Z",
        "requestId":"request", "message":{"id":id,"model":"claude-3-5-sonnet",
        "usage":{"input_tokens":input,"output_tokens":3}}})
}

fn visit(path: &Path, home: Option<&Path>) -> Vec<UnifiedMessage> {
    let mut rows = Vec::new();
    visit_claude_file_with_home(path, home, &mut |message| {
        rows.push(message);
        Ok(())
    })
    .unwrap();
    rows
}

#[test]
fn unchanged_and_mutated_stream_preserve_first_order_max_confidence_and_turn_cursor() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join(".claude/projects/-workspace-project");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.join("session.jsonl");
    let mut rows = vec![
        serde_json::json!({"type":"user","timestamp":"2024-12-01T10:00:00Z",
            "message":{"content":"hello"}}),
        assistant("a", 2),
        assistant("b", 4),
        serde_json::json!({"type":"user","timestamp":"2024-12-01T10:00:02Z",
            "message":{"content":[{"type":"tool_result","tool_use_id":"tool",
                "input_tokens":7,"content":"explicit result"}]}}),
    ];
    write_rows(&path, &rows);
    let initial = visit(&path, Some(root.path()));
    assert_eq!(initial.len(), 3);
    assert_eq!(initial[0].dedup_key.as_deref(), Some("a:request"));
    assert_eq!(initial[1].dedup_key.as_deref(), Some("b:request"));
    assert_eq!(
        initial[2].dedup_key.as_deref(),
        Some("claude:tool_result:session:tool_result:tool")
    );
    assert!(initial[0].is_turn_start);
    assert!(!initial[1].is_turn_start);
    assert_eq!(initial[0].duration_ms, Some(1000));
    assert_eq!(initial[1].duration_ms, None);
    assert_eq!(initial[2].tokens.input, 7);
    assert!(initial[0].workspace_key.is_some());
    assert_eq!(initial, visit(&path, Some(root.path())));
    assert_eq!(
        initial,
        parse_claude_file_with_home(&path, Some(root.path()))
    );

    rows.push(
        serde_json::json!({"type":"user","timestamp":"2024-12-01T10:00:03Z",
        "message":{"content":"next human turn"}}),
    );
    rows.push(
        serde_json::json!({"type":"assistant","timestamp":"2024-12-01T10:00:05Z",
        "requestId":"request","providerId":"openrouter","message":{"id":"a",
        "usage":{"input_tokens":9,"output_tokens":1,"cache_creation_input_tokens":6,
            "cache_creation":{"ephemeral_1h_input_tokens":4}}}}),
    );
    let mut duplicate_tool = rows[3].clone();
    duplicate_tool["timestamp"] = serde_json::json!("2024-12-01T10:00:04Z");
    duplicate_tool["message"]["content"][0]["input_tokens"] = serde_json::json!(11);
    rows.push(duplicate_tool);
    let mut last = assistant("c", 8);
    last["timestamp"] = serde_json::json!("2024-12-01T10:00:06Z");
    rows.push(last);
    write_rows(&path, &rows);
    let mutated = visit(&path, Some(root.path()));
    assert_eq!(mutated.len(), 4);
    assert_eq!(mutated[0].tokens.input, 9);
    assert_eq!(mutated[0].tokens.output, 3);
    assert_eq!(mutated[0].tokens.cache_write, 6);
    assert_eq!(mutated[0].tokens.cache_write_1h, 4);
    assert_eq!(mutated[0].provider_id, "openrouter");
    assert_eq!(mutated[0].timestamp, initial[0].timestamp);
    assert_eq!(mutated[0].duration_ms, Some(5000));
    assert_eq!(mutated[1], initial[1]);
    assert_eq!(mutated[2].tokens.input, 11);
    assert_eq!(
        mutated[2].timestamp,
        parse_claude_entry_timestamp(Some("2024-12-01T10:00:04Z")).unwrap()
    );
    assert_eq!(mutated[3].dedup_key.as_deref(), Some("c:request"));
    assert!(mutated[3].is_turn_start);
    assert_eq!(mutated[3].duration_ms, Some(2000));
    assert_eq!(
        mutated,
        parse_claude_file_with_home(&path, Some(root.path()))
    );
}

#[test]
fn headless_projection_keeps_presence_precedence_and_existing_interpretation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("headless.json");
    for value in [
        serde_json::json!({"model":"claude-3-5-sonnet","usage":{"input_tokens":"12",
            "output_tokens":4,"cache_creation_input_tokens":6,
            "cache_creation":{"ephemeral_1h_input_tokens":9}},"result":"ignored body",
            "message":{"model":"other","usage":{"input_tokens":100}},
            "provider":"openrouter","timestamp":"2024-12-01T10:00:00Z"}),
        serde_json::json!({"usage":null,"model":null,"providerId":null,
            "message":{"model":"claude-3-5-sonnet","provider_id":"bedrock",
            "usage":{"input_tokens":100},"created_at":"2024-12-01T10:00:00Z"}}),
        serde_json::json!({"message":{"model":"claude-3-5-sonnet",
            "usage":{"input_tokens":7,"output_tokens":8},"created_at":1733047200000i64},
            "timestamp":null}),
        serde_json::json!({"model":"<synthetic>","usage":{"input_tokens":9}}),
    ] {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let original: Value = simd_json::from_slice(&mut bytes).unwrap();
        let expected = extract_claude_headless_message(&original, "headless", 123, "claude", None);
        let actual = parse_claude_headless_json(&path, "headless", 123, "claude", None).unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn wrapper_events_malformed_rows_and_undecodable_rows_keep_skip_semantics() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.jsonl");
    let content = concat!(
        "{\"type\":\"message_start\",\"message\":{\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":5}}}\n",
        "{\"type\":\"message_delta\",\"usage\":{\"output_tokens\":8}}\n",
        "{\"type\":\"message_stop\"}\n",
        "malformed\n",
        "{\"type\":\"result\",\"model\":\"claude-3-5-sonnet\",\"provider\":\"bedrock\",\"usage\":{\"input_tokens\":3}}\n"
    );
    let mut bytes = content.as_bytes().to_vec();
    bytes.extend_from_slice(b"\xff\n");
    bytes.extend_from_slice(
        serde_json::to_string(&assistant("after-invalid", 4))
            .unwrap()
            .as_bytes(),
    );
    std::fs::write(&path, bytes).unwrap();
    let rows = visit(&path, None);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].tokens.input, 5);
    assert_eq!(rows[0].tokens.output, 8);
    assert_eq!(rows[1].tokens.input, 3);
    assert_eq!(rows[1].provider_id, "bedrock");
    assert_eq!(rows[2].dedup_key.as_deref(), Some("after-invalid:request"));
}

#[test]
fn parent_join_spills_and_sidecar_mutation_keeps_resolution_precedence() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join(".claude/projects/project");
    let subagents = project.join("parent/subagents/workflows/workflow");
    std::fs::create_dir_all(&subagents).unwrap();
    let parent = project.join("parent.jsonl");
    let mut file = std::fs::File::create(&parent).unwrap();
    for index in 0..12000 {
        let row = serde_json::json!({"type":"user","message":{"content":[{
            "type":"tool_result","tool_use_id":format!("tool{index}"),
            "content":[{"text":format!("agentId: agent{index} done")}]}]}});
        serde_json::to_writer(&mut file, &row).unwrap();
        writeln!(file).unwrap();
    }
    for index in 0..12000 {
        let row = serde_json::json!({"type":"assistant","message":{"content":[{
            "type":"tool_use","id":format!("tool{index}"),
            "input":{"subagent_type":"explore"}}]}});
        serde_json::to_writer(&mut file, &row).unwrap();
        writeln!(file).unwrap();
    }
    drop(file);
    let path = subagents.join("agent-aside-agent11999.jsonl");
    let mut row = assistant("side", 20);
    row["isSidechain"] = serde_json::json!(true);
    row["sessionId"] = serde_json::json!("parent");
    row["agentId"] = serde_json::json!("agent11999");
    write_rows(&path, &[row]);
    let initial = visit(&path, Some(root.path()));
    assert_eq!(initial[0].agent.as_deref(), Some("Explore"));
    assert_eq!(initial[0].session_id, "parent");
    let sidecar = path.with_file_name("agent-aside-agent11999.meta.json");
    std::fs::write(&sidecar, r#"{"agentType":"writer"}"#).unwrap();
    assert_eq!(
        visit(&path, Some(root.path()))[0].agent.as_deref(),
        Some("Writer")
    );
    std::fs::write(&sidecar, "malformed").unwrap();
    assert_eq!(
        visit(&path, Some(root.path()))[0].agent.as_deref(),
        Some("Explore")
    );
    let mut retained = initial[0].clone();
    std::fs::write(&sidecar, r#"{"agentType":"executor"}"#).unwrap();
    refresh_retained_message_context(&mut retained, &path, Some(root.path()), None);
    assert_eq!(retained.agent.as_deref(), Some("Executor"));
    assert_eq!(retained.dedup_key, initial[0].dedup_key);
}

struct InterruptedSource(Cursor<Vec<u8>>);
impl Read for InterruptedSource {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.0.position() == self.0.get_ref().len() as u64 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected read failure",
            ));
        }
        self.0.read(output)
    }
}

#[test]
fn partial_read_never_emits_and_sink_failure_never_reports_complete_success() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.jsonl");
    let mut bytes = serde_json::to_vec(&assistant("a", 2)).unwrap();
    bytes.push(b'\n');
    let mut cache = ParentSubagentTypeCache::new();
    let mut emitted = 0;
    let result = visit_claude_reader(
        &path,
        &mut cache,
        None,
        BufReader::new(InterruptedSource(Cursor::new(bytes))),
        &mut |_| {
            emitted += 1;
            Ok(())
        },
    );
    assert!(result.unwrap_err().contains("injected read failure"));
    assert_eq!(emitted, 0);
    write_rows(&path, &[assistant("a", 2), assistant("b", 3)]);
    let error = visit_claude_file_with_home(&path, None, &mut |_| {
        emitted += 1;
        Err("sink rejected generation".to_string())
    })
    .unwrap_err();
    assert_eq!(error, "sink rejected generation");
    assert_eq!(emitted, 1);
    assert!(
        visit_claude_file_with_home(&root.path().join("absent.jsonl"), None, &mut |_| Ok(()))
            .is_err()
    );
}

#[test]
fn large_headless_body_is_streamed_and_large_individual_line_is_not_dropped() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("large.json");
    let mut file = std::fs::File::create(&path).unwrap();
    write!(file, "{{\"result\":\"").unwrap();
    let chunk = [b'x'; 8192];
    for _ in 0..2048 {
        file.write_all(&chunk).unwrap();
    }
    write!(
        file,
        "\",\"model\":\"claude-3-5-sonnet\",\"usage\":{{\"input_tokens\":123}}}}"
    )
    .unwrap();
    drop(file);
    let rows = visit(&path, None);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens.input, 123);
    let line_path = root.path().join("large-line.jsonl");
    let mut row = assistant("large-record", 456);
    row["message"]["content"] = serde_json::json!("x".repeat(8 * 1024 * 1024 + 1));
    write_rows(&line_path, &[row]);
    assert_eq!(visit(&line_path, None)[0].tokens.input, 456);
}

#[test]
fn large_history_is_finalized_on_actual_disk_with_bounded_cache() {
    let rows = ClaudeRows::new(ClaudeStorage::Disk).unwrap();
    let cache_size: i64 = rows
        .0
        .query_row("PRAGMA scratch.cache_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(cache_size, -2048);
    for index in 0..24000 {
        let message = UnifiedMessage::new_with_dedup(
            "claude",
            "claude-3-5-sonnet",
            "anthropic",
            "session",
            index,
            TokenBreakdown {
                input: index,
                ..Default::default()
            },
            0.0,
            Some(format!("key{index}")),
        );
        rows.push(message, CLAUDE_PROVIDER_DEFAULT_CONFIDENCE)
            .unwrap();
    }
    let (position, mut winner, confidence) = rows.get("key0").unwrap().unwrap();
    winner.tokens.input = 999;
    rows.update(position, &winner, confidence).unwrap();
    let mut count = 0i64;
    rows.emit(&mut |row| {
        assert_eq!(row.timestamp, count);
        assert_eq!(row.tokens.input, if count == 0 { 999 } else { count });
        count += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 24000);
    // The VFS file pointer and its nonzero on-disk size prove this is not an
    // in-memory btree merely returning temp_store=FILE from a pragma.
    // SAFETY: the scratch connection remains alive, the file-control output is
    // checked before dereference, and xFileSize receives its own live VFS file.
    unsafe {
        let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
        let result = rusqlite::ffi::sqlite3_file_control(
            rows.0.handle(),
            c"scratch".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
        );
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        assert!(!file.is_null());
        assert!(!(*file).pMethods.is_null());
        let mut size = 0;
        let result = ((*(*file).pMethods).xFileSize.unwrap())(file, &mut size);
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        assert!(size > 2 * 1024 * 1024);
    }
}

#[test]
fn cc_mirror_variant_mutation_refreshes_client_provider_workspace_without_rekeying() {
    let root = tempfile::tempdir().unwrap();
    let variant = root.path().join(".cc-mirror/variant");
    let project = variant.join("config/projects/-workspace-mirror");
    std::fs::create_dir_all(&project).unwrap();
    let metadata = variant.join("variant.json");
    let path = project.join("session.jsonl");
    write_rows(&path, &[assistant("stable", 42)]);
    std::fs::write(
        &metadata,
        r#"{"name":"First Mirror","provider":"openrouter"}"#,
    )
    .unwrap();
    let initial = visit(&path, Some(root.path()));
    assert_eq!(initial[0].client, "cc-mirror/first-mirror");
    assert_eq!(initial[0].provider_id, "openrouter");
    assert_eq!(
        initial[0].workspace_key.as_deref(),
        Some("-workspace-mirror")
    );
    assert_eq!(initial, visit(&path, Some(root.path())));
    std::fs::write(
        &metadata,
        r#"{"name":"Second Mirror","provider":"bedrock"}"#,
    )
    .unwrap();
    let changed = visit(&path, Some(root.path()));
    assert_eq!(changed[0].client, "cc-mirror/second-mirror");
    assert_eq!(changed[0].provider_id, "bedrock");
    assert_eq!(changed[0].dedup_key, initial[0].dedup_key);
    assert_eq!(changed[0].tokens, initial[0].tokens);
    assert_eq!(
        changed,
        parse_claude_file_with_home(&path, Some(root.path()))
    );
    let mut retained = initial[0].clone();
    refresh_retained_message_context(&mut retained, &path, Some(root.path()), None);
    assert_eq!(retained, changed[0]);
}

#[test]
fn ambiguous_parent_agent_join_selects_only_an_original_possible_winner() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent.jsonl");
    write_rows(
        &parent,
        &[
            serde_json::json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"first","input":{"subagent_type":"writer"}},
                {"type":"tool_use","id":"second","input":{"subagent_type":"explore"}}
            ]}}),
            serde_json::json!({"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"first","content":[{"text":"agentId: same"}]},
                {"type":"tool_result","tool_use_id":"second","content":[{"text":"agentId: same"}]}
            ]}}),
        ],
    );
    let mut cache = ParentSubagentTypeCache::new();
    let actual = lookup_subagent_type_in_parent_complete(&parent, "same", &mut cache)
        .unwrap()
        .unwrap();
    assert!(["writer", "explore"].contains(&actual.as_str()));
}

#[test]
fn json_extension_keeps_jsonl_fallback_and_trailing_validation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("fallback.json");
    write_rows(&path, &[assistant("first", 12), assistant("second", 13)]);
    let rows = visit(&path, None);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].dedup_key.as_deref(), Some("first:request"));
    assert_eq!(rows[1].dedup_key.as_deref(), Some("second:request"));
    assert_eq!(rows[0].tokens.input, 12);
    assert_eq!(rows[1].tokens.input, 13);
}

#[test]
fn unreadable_sidecar_aborts_source_instead_of_publishing_generic_agent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agent-side.jsonl");
    let mut row = assistant("side", 5);
    row["isSidechain"] = serde_json::json!(true);
    row["sessionId"] = serde_json::json!("parent");
    write_rows(&path, &[row]);
    std::fs::create_dir(root.path().join("agent-side.meta.json")).unwrap();
    let mut emitted = 0;
    let result = visit_claude_file_with_home(&path, None, &mut |_| {
        emitted += 1;
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(emitted, 0);
}

#[test]
fn raw_headless_scalar_validation_matches_original_simd() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("invalid.json");
    for (raw, expected_input) in [
        (br#"{"result":"\uD800","model":"claude-3-5-sonnet","usage":{"input_tokens":123}}"#.as_slice(), Some(123)),
        (br#"{"result":"\uDC00","model":"claude-3-5-sonnet","usage":{"input_tokens":123}}"#.as_slice(), None),
        (br#"{"result":"\uD800\u0041","model":"claude-3-5-sonnet","usage":{"input_tokens":123}}"#.as_slice(), None),
        (b"{\"result\":\"\xff\",\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":123}}".as_slice(), None),
        (br#"{"result":1e400,"model":"claude-3-5-sonnet","usage":{"input_tokens":123}}"#.as_slice(), None),
        (br#"{"result":{"nested":[18446744073709551616]},"model":"claude-3-5-sonnet","usage":{"input_tokens":123}}"#.as_slice(), None),
        (br#"{"model":"claude-3-5-sonnet","usage":{"input_tokens":18446744073709551616,"output_tokens":3}}"#.as_slice(), None),
        (br#"{"model":"claude-3-5-sonnet","usage":{"input_tokens":-9223372036854775809,"output_tokens":3}}"#.as_slice(), None),
    ] {
        let mut original = raw.to_vec();
        let original = simd_json::from_slice::<Value>(&mut original);
        assert_eq!(original.is_ok(), expected_input.is_some(), "{raw:?}");
        let expected = original.ok().and_then(|value| {
            extract_claude_headless_message(&value, "invalid", 123, "claude", None)
        });
        assert_eq!(expected.as_ref().map(|message| message.tokens.input), expected_input);
        std::fs::write(&path, raw).unwrap();
        assert_eq!(
            parse_claude_headless_json(&path, "invalid", 123, "claude", None).unwrap(),
            expected,
            "{raw:?}"
        );
        let visited = visit(&path, None);
        assert_eq!(visited.len(), usize::from(expected_input.is_some()));
        if let Some(expected) = expected {
            assert_eq!(visited[0].tokens, expected.tokens);
        }
    }
}

#[test]
fn accepted_legacy_unicode_spelling_survives_multiline_and_kept_fields() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("legacy.json");
    for (escape, decoded) in [
        (r"\uD800", "\0"),
        (r"\uD800x", "\0x"),
        (r"\uD800\n", "\0\n"),
        (r"\uD800\uE000", "\u{10400}"),
    ] {
        let raw = format!(
            "{{\n\"result\":\"{escape}\",\n\"model\":\"model-{escape}\",\n\
             \"provider\":\"openrouter\",\n\"timestamp\":123,\n\
             \"usage\":{{\"input_tokens\":123}}\n}}"
        );
        let mut original = raw.as_bytes().to_vec();
        let original: Value = simd_json::from_slice(&mut original).unwrap();
        assert_eq!(original["model"], format!("model-{decoded}"));
        let expected =
            extract_claude_headless_message(&original, "legacy", 123, "claude", None).unwrap();
        std::fs::write(&path, &raw).unwrap();
        assert_eq!(
            parse_claude_headless_json(&path, "legacy", 123, "claude", None)
                .unwrap()
                .unwrap(),
            expected
        );
        assert_eq!(
            visit(&path, None).as_slice(),
            std::slice::from_ref(&expected)
        );
        for maximum in 1..=5 {
            let reader = ValidatedHeadlessReader::new(TinyReads {
                bytes: raw.as_bytes(),
                maximum,
            });
            let normalized: Value = serde_json::from_reader(reader).unwrap();
            assert_eq!(normalized, original);
        }
    }
}

struct TinyReads<'a> {
    bytes: &'a [u8],
    maximum: usize,
}

impl Read for TinyReads<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let count = output.len().min(self.maximum).min(self.bytes.len());
        output[..count].copy_from_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        Ok(count)
    }
}

#[test]
fn streamed_scalar_validation_preserves_valid_boundary_utf8_escapes_numbers_and_duplicate_keys() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("valid.json");
    let raw = br#"{"result":"\uD83D\uDE00 \\ \" \n","ignored":[18446744073709551615,-9223372036854775808,1e300],"model":"other","model":"claude-3-5-sonnet","usage":{"input_tokens":1},"usage":{"input_tokens":2,"output_tokens":3}}"#;
    let mut source = raw.to_vec();
    let original: Value = simd_json::from_slice(&mut source).unwrap();
    std::fs::write(&path, raw).unwrap();
    assert_eq!(
        parse_claude_headless_json(&path, "valid", 123, "claude", None).unwrap(),
        extract_claude_headless_message(&original, "valid", 123, "claude", None)
    );
    let utf8 =
        "{\"result\":\"😀ห🦀\",\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":2}}";
    for raw in [raw.as_slice(), utf8.as_bytes()] {
        for maximum in 1..=5 {
            let mut reader = ValidatedHeadlessReader::new(TinyReads {
                bytes: raw,
                maximum,
            });
            let mut copied = Vec::new();
            reader.read_to_end(&mut copied).unwrap();
            assert_eq!(copied, raw);
            assert!(!reader.invalid);
        }
    }
}

#[test]
fn wrong_typed_headless_fields_do_not_materialize_nested_bodies_or_change_fallback() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("wrong-type.json");
    let mut file = std::fs::File::create(&path).unwrap();
    write!(file, "{{\"model\":{{\"body\":\"").unwrap();
    let chunk = [b'x'; 8192];
    for _ in 0..2048 {
        file.write_all(&chunk).unwrap();
    }
    write!(
        file,
        "\"}},\"message\":{{\"model\":\"claude-3-5-sonnet\"}},\"usage\":{{\"input_tokens\":8}}}}"
    )
    .unwrap();
    drop(file);
    let rows = visit(&path, None);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model_id, "claude-3-5-sonnet");
    assert_eq!(rows[0].tokens.input, 8);
}

#[test]
fn wrong_scalar_container_depth_has_no_new_value_recursion_limit() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("deep.json");
    let nested = format!("{}null{}", "[".repeat(256), "]".repeat(256));
    let raw = format!(
        "{{\n\"model\":\"claude-3-5-sonnet\",\n\"timestamp\":{nested},\n\
         \"usage\":{{\"input_tokens\":{nested},\"output_tokens\":7}}\n}}"
    );
    let mut bytes = raw.as_bytes().to_vec();
    let original: Value = simd_json::from_slice(&mut bytes).unwrap();
    std::fs::write(&path, raw).unwrap();
    let expected = extract_claude_headless_message(&original, "deep", 123, "claude", None);
    let actual = parse_claude_headless_json(&path, "deep", 123, "claude", None).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.unwrap().tokens.output, 7);
}

struct DiskFailureGuard(Option<ClaudeDiskFailure>);

impl DiskFailureGuard {
    fn inject(failure: ClaudeDiskFailure) -> Self {
        Self(CLAUDE_DISK_FAILURE.with(|current| current.replace(Some(failure))))
    }
}

impl Drop for DiskFailureGuard {
    fn drop(&mut self) {
        CLAUDE_DISK_FAILURE.with(|current| {
            current.replace(self.0.take());
        });
    }
}

#[test]
fn actual_sqlite_open_and_full_failures_abort_disk_ingestion_but_not_materialization() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    for index in 0..2000 {
        serde_json::to_writer(&mut file, &assistant(&format!("message{index}"), index)).unwrap();
        writeln!(file).unwrap();
    }
    serde_json::to_writer(&mut file, &assistant("message0", 7000)).unwrap();
    writeln!(file).unwrap();
    drop(file);
    let expected = parse_claude_file_with_home(&path, None);
    assert_eq!(expected.len(), 2000);
    assert_eq!(expected[0].tokens.input, 7000);
    let impossible = root
        .path()
        .join("absent-directory/scratch.sqlite3")
        .to_string_lossy()
        .into_owned();
    for failure in [ClaudeDiskFailure::Open(impossible), ClaudeDiskFailure::Full] {
        let full = matches!(&failure, ClaudeDiskFailure::Full);
        let _guard = DiskFailureGuard::inject(failure);
        let mut emitted = 0;
        let error = visit_claude_file_with_home(&path, None, &mut |_| {
            emitted += 1;
            Ok(())
        })
        .unwrap_err();
        assert!(if full {
            error.contains("full")
        } else {
            error.contains("unable to open")
        });
        assert_eq!(emitted, 0);
        assert_eq!(parse_claude_file_with_home(&path, None), expected);
        let mut parent_cache = ParentSubagentTypeCache::new();
        assert_eq!(
            parse_claude_file_with_cache_and_home(&path, &mut parent_cache, None),
            expected
        );
    }
}

#[test]
fn materialized_parent_lookup_survives_actual_disk_full_with_same_join_interpretation() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("parent.jsonl");
    let mut file = std::fs::File::create(&parent).unwrap();
    for index in 0..2000 {
        let row = serde_json::json!({"type":"assistant","message":{"content":[{
            "type":"tool_use","id":format!("tool{index}"),"input":{"subagent_type":"explore"}
        }]}});
        serde_json::to_writer(&mut file, &row).unwrap();
        writeln!(file).unwrap();
    }
    writeln!(
        file,
        "{}",
        serde_json::json!({"type":"user","message":{"content":[{
            "type":"tool_result","tool_use_id":"tool1999","content":[{"text":"agentId: edge"}]
        }]}})
    )
    .unwrap();
    drop(file);
    let path = root.path().join("agent-edge.jsonl");
    let mut row = assistant("side", 20);
    row["isSidechain"] = serde_json::json!(true);
    row["sessionId"] = serde_json::json!("parent");
    row["agentId"] = serde_json::json!("edge");
    write_rows(&path, &[row]);
    let expected = parse_claude_file_with_home(&path, None);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].agent.as_deref(), Some("Explore"));
    let _guard = DiskFailureGuard::inject(ClaudeDiskFailure::Full);
    let mut emitted = 0;
    let error = visit_claude_file_with_home(&path, None, &mut |_| {
        emitted += 1;
        Ok(())
    })
    .unwrap_err();
    assert!(error.contains("full"));
    assert_eq!(emitted, 0);
    assert_eq!(parse_claude_file_with_home(&path, None), expected);
}

#[test]
fn duplicate_input_assembly_preserves_units_and_presence() {
    // (first usage, second usage, expected input, expected split)
    let cases = [
        (
            serde_json::json!({"input_tokens": 42494}),
            serde_json::json!({"input_tokens": 265, "cache_read_input_tokens": 42000, "output_tokens": 120}),
            265,
            true,
        ),
        (
            serde_json::json!({"input_tokens": 42494}),
            serde_json::json!({"input_tokens": 30, "cache_creation_input_tokens": 0}),
            30,
            true,
        ),
        (
            serde_json::json!({"input_tokens": 10}),
            serde_json::json!({"input_tokens": 50, "cache_read_input_tokens": null}),
            50,
            false,
        ),
        (
            serde_json::json!({"input_tokens": 10}),
            serde_json::json!({"input_tokens": 50}),
            50,
            false,
        ),
        (
            serde_json::json!({"input_tokens": 10, "cache_read_input_tokens": 0}),
            serde_json::json!({"input_tokens": 50, "cache_creation_input_tokens": 0}),
            50,
            true,
        ),
    ];
    for (first, second, expected_input, expected_split) in cases {
        for reverse in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let row = |usage: serde_json::Value| {
                serde_json::json!({"type": "assistant", "timestamp": "2026-01-01T10:00:00Z",
                    "requestId": "req_split",
                    "message": {"id": "msg_split", "model": "claude-sonnet-4-6", "usage": usage}})
            };
            let mut rows = vec![row(first.clone()), row(second.clone())];
            if reverse {
                rows.reverse();
            }
            write_rows(&path, &rows);
            let messages = parse_claude_file_with_home(&path, None);
            assert_eq!(messages.len(), 1, "reverse={reverse}");
            assert_eq!(
                messages[0].tokens.input, expected_input,
                "reverse={reverse}"
            );
            assert_eq!(
                messages[0].input_tokens_are_cache_split, expected_split,
                "reverse={reverse}"
            );
            assert_eq!(
                messages[0].dedup_key.as_deref(),
                Some("msg_split:req_split")
            );
            if expected_input == 265 {
                assert_eq!(messages[0].tokens.total(), 42385);
            }
        }
    }
}

#[test]
fn a_silent_split_does_not_claim_the_bare_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let bare = serde_json::json!({"type": "assistant", "timestamp": "2026-01-01T10:00:00Z",
        "requestId": "req_split",
        "message": {"id": "msg_split", "model": "claude-sonnet-4-6",
        "usage": {"input_tokens": 42494}}});
    let silent = serde_json::json!({"type": "assistant", "timestamp": "2026-01-01T10:00:01Z",
        "requestId": "req_split",
        "message": {"id": "msg_split", "model": "claude-sonnet-4-6",
        "usage": {"cache_read_input_tokens": 42000, "output_tokens": 120}}});
    write_rows(&path, &[bare.clone(), silent.clone()]);
    let messages = parse_claude_file_with_home(&path, None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tokens.input, 42494);
    assert!(!messages[0].input_tokens_are_cache_split);
    let split = serde_json::json!({"type": "assistant", "timestamp": "2026-01-01T10:00:02Z",
        "requestId": "req_split",
        "message": {"id": "msg_split", "model": "claude-sonnet-4-6",
        "usage": {"input_tokens": 265, "cache_read_input_tokens": 42000}}});
    write_rows(&path, &[bare, silent, split]);
    let messages = parse_claude_file_with_home(&path, None);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tokens.total(), 42385);
    assert!(messages[0].input_tokens_are_cache_split);
}
