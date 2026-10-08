use chrono::{Duration, Local, TimeZone};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use syrtis_core::{
    get_hourly_report, parse_local_clients, visit_recent_local_messages, LocalParseOptions,
    ParsedMessage, ReportOptions, UnifiedMessage,
};

struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn isolate(home: &Path, cache: &Path) -> Self {
        let values = [
            ("HOME", home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache.as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", OsStr::new("1")),
        ];
        let previous = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        unsafe {
            for (key, value) in values {
                std::env::set_var(key, value);
            }
        }
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Event {
    timestamp: i64,
    client: String,
    agent: String,
    model: String,
    tokens: [i64; 5],
    messages: i32,
}

fn parsed_event(message: &ParsedMessage) -> Event {
    Event {
        timestamp: message.timestamp,
        client: message.client.clone(),
        agent: message
            .agent
            .clone()
            .unwrap_or_else(|| message.client.clone()),
        model: message.model_id.clone(),
        tokens: [
            message.input,
            message.output,
            message.reasoning,
            message.cache_read,
            message.cache_write,
        ],
        messages: message.message_count,
    }
}

fn unified_event(message: &UnifiedMessage) -> Event {
    Event {
        timestamp: message.timestamp,
        client: message.client.clone(),
        agent: message
            .agent
            .clone()
            .unwrap_or_else(|| message.client.clone()),
        model: message.model_id.clone(),
        tokens: [
            message.tokens.input,
            message.tokens.output,
            message.tokens.reasoning,
            message.tokens.cache_read,
            message.tokens.cache_write,
        ],
        messages: message.message_count,
    }
}

fn options(home: &Path, clients: &[&str]) -> LocalParseOptions {
    LocalParseOptions {
        home_dir: Some(home.to_string_lossy().into_owned()),
        clients: Some(clients.iter().map(|client| (*client).to_string()).collect()),
        ..Default::default()
    }
}

fn recent(options: LocalParseOptions, cutoff: i64) -> Vec<Event> {
    let mut events = Vec::new();
    visit_recent_local_messages(options, cutoff, &mut |message| {
        events.push(unified_event(message))
    })
    .unwrap();
    events.sort();
    events
}

fn legacy(options: LocalParseOptions, cutoff: i64) -> Vec<Event> {
    let mut events: Vec<_> = parse_local_clients(options)
        .unwrap()
        .messages
        .iter()
        .filter(|message| message.timestamp >= cutoff)
        .map(parsed_event)
        .collect();
    events.sort();
    events
}

type Trace = BTreeMap<(String, String, String), (i64, u32)>;

fn rate_and_trace(events: &[Event], cutoff: i64, seconds: i64) -> (f32, Trace) {
    let mut total = 0_i64;
    let mut groups = BTreeMap::new();
    for event in events.iter().filter(|event| event.timestamp >= cutoff) {
        let tokens = event.tokens.into_iter().fold(0_i64, i64::saturating_add);
        total = total.saturating_add(tokens);
        let slot = groups
            .entry((
                event.client.clone(),
                event.agent.clone(),
                event.model.clone(),
            ))
            .or_insert((0_i64, 0_u32));
        slot.0 = slot.0.saturating_add(tokens);
        slot.1 = slot.1.saturating_add(event.messages.max(0) as u32);
    }
    (total as f32 / (seconds as f32 / 60.0), groups)
}

fn write(path: &Path, content: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn opencode(id: &str, timestamp: i64, input: i64, cost: f64) -> serde_json::Value {
    json!({
        "id": id, "sessionID": "session", "role": "assistant", "modelID": "gpt-4o",
        "providerID": "openai", "mode": "build", "cost": cost,
        "time": {"created": timestamp},
        "tokens": {"input": input, "output": 2, "reasoning": 3, "cache": {"read": 5, "write": 7}}
    })
}

#[test]
#[serial_test::serial]
fn recent_events_match_materialized_rate_and_trace_across_midnight_and_expiry() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let midnight = Local
        .with_ymd_and_hms(2026, 2, 10, 0, 0, 0)
        .single()
        .unwrap();
    let cutoff = midnight.timestamp_millis() - 30 * 60 * 1000;
    let dir = home
        .path()
        .join(".local/share/opencode/storage/message/session");
    for (id, timestamp, input) in [
        ("expired", cutoff - 1, 1000),
        ("boundary", cutoff, 11),
        ("today", midnight.timestamp_millis() + 1, 13),
    ] {
        write(
            &dir.join(format!("{id}.json")),
            opencode(id, timestamp, input, 0.01).to_string(),
        );
    }
    // Codebuff's local-count lane deliberately retains copies with the same
    // upstream id; report streaming has a different cross-file dedup contract.
    for project in ["a", "b"] {
        write(
            &home.path().join(format!(
                ".config/manicode/projects/{project}/chat-messages.json"
            )),
            json!([{"role":"assistant", "id":"shared", "timestamp":midnight.to_rfc3339(),
                "metadata":{"model":"gpt-4o", "usage":{"inputTokens":17,"outputTokens":19,
                    "cacheReadInputTokens":23,"cacheCreationInputTokens":29}}}])
            .to_string(),
        );
    }
    let mut opts = options(home.path(), &["opencode", "codebuff"]);
    opts.since = Some(
        (midnight - Duration::days(1))
            .format("%Y-%m-%d")
            .to_string(),
    );
    opts.modified_after = Some(cutoff as u64);
    let expected = legacy(opts.clone(), cutoff);
    assert_eq!(expected.len(), 4);
    assert_eq!(
        expected
            .iter()
            .filter(|event| event.client == "codebuff")
            .count(),
        2
    );
    for _ in 0..2 {
        let actual = recent(opts.clone(), cutoff);
        assert_eq!(actual, expected);
        assert_eq!(
            rate_and_trace(&actual, cutoff, 3600),
            rate_and_trace(&expected, cutoff, 3600)
        );
        let later_cutoff = midnight.timestamp_millis();
        assert_eq!(
            rate_and_trace(&actual, later_cutoff, 60),
            rate_and_trace(&expected, later_cutoff, 60)
        );
        let expired_cutoff = midnight.timestamp_millis() + 3600 * 1000;
        assert_eq!(rate_and_trace(&actual, expired_cutoff, 3600).0, 0.0);
        assert!(rate_and_trace(&actual, expired_cutoff, 3600).1.is_empty());
    }
    assert!(recent(options(home.path(), &["codex"]), cutoff).is_empty());
    let mut gateway = opencode("gateway", midnight.timestamp_millis(), 31, 0.0);
    gateway["modelID"] = json!("hf:org/Gateway-Test");
    gateway["providerID"] = json!("unknown");
    write(&dir.join("gateway.json"), gateway.to_string());
    for clients in [None, Some(Vec::new())] {
        let mut all = opts.clone();
        all.clients = clients;
        let expected = legacy(all.clone(), cutoff);
        assert!(expected.iter().any(|event| event.model == "gateway-test"));
        assert_eq!(
            recent(all, cutoff),
            expected,
            "nil and empty still select all local clients"
        );
    }
}

#[test]
#[serial_test::serial]
fn recent_selection_preserves_legacy_authority_without_changing_report_fallback() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let midnight = Local
        .with_ymd_and_hms(2026, 2, 10, 0, 0, 0)
        .single()
        .unwrap()
        .timestamp_millis();
    let data = home.path().join(".local/share/opencode");
    std::fs::create_dir_all(&data).unwrap();
    let db = rusqlite::Connection::open(data.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES (?1, ?2, ?3)",
        rusqlite::params![
            "row",
            "session",
            opencode("shared", midnight + 1, 111, 0.0).to_string()
        ],
    )
    .unwrap();
    drop(db);
    let authority = data.join("storage/message/session/authority.json");
    write(
        &authority,
        opencode("shared", midnight - 1, 222, 0.25).to_string(),
    );
    let mut opts = options(home.path(), &["opencode"]);
    opts.since = Some("2026-02-10".to_string());
    // Creation time is part of source identity: the same ID at a different
    // timestamp is not an authoritative sibling of the recent SQLite row.
    let expected = legacy(opts.clone(), midnight);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].tokens[0], 111);
    assert_eq!(recent(opts.clone(), midnight), expected);
    assert!(recent(opts.clone(), midnight + 2).is_empty());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: opts.home_dir.clone(),
            clients: opts.clients.clone(),
            since: opts.since.clone(),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        report.entries.iter().map(|entry| entry.input).sum::<i64>(),
        111,
        "report date filtering also preserves the distinct recent identity"
    );
    let before = syrtis_core::local_source_change_token(&opts).unwrap();
    write(
        &authority,
        opencode("shared", midnight + 1, 333, 0.5).to_string(),
    );
    assert_ne!(
        syrtis_core::local_source_change_token(&opts).unwrap(),
        before
    );
    let expected = legacy(opts.clone(), midnight);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].tokens[0], 333);
    assert_eq!(recent(opts.clone(), midnight), expected);
    assert!(recent(opts.clone(), midnight + 2).is_empty());

    // At the same creation time these copies really do share source identity.
    // A Synthetic-only client gate filters out the authoritative non-gateway
    // JSON winner. Materialized selection must not resurrect the gateway
    // estimate; reports deliberately retain that query-dependent fallback.
    let mut gateway = opencode("shared", midnight + 1, 111, 0.0);
    gateway["modelID"] = json!("hf:org/Gateway-Test");
    gateway["providerID"] = json!("unknown");
    let db = rusqlite::Connection::open(data.join("opencode.db")).unwrap();
    db.execute(
        "UPDATE message SET data = ?1 WHERE id = 'row'",
        [gateway.to_string()],
    )
    .unwrap();
    drop(db);
    opts.clients = Some(vec!["synthetic".to_string()]);
    assert!(legacy(opts.clone(), midnight).is_empty());
    assert!(recent(opts.clone(), midnight).is_empty());
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: opts.home_dir.clone(),
            clients: opts.clients.clone(),
            since: opts.since.clone(),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        report.entries.iter().map(|entry| entry.input).sum::<i64>(),
        111,
        "report client filtering still falls back to the gateway SQLite copy"
    );
    gateway["tokens"]["input"] = json!(444);
    gateway["cost"] = json!(0.5);
    write(&authority, gateway.to_string());
    let expected = legacy(opts.clone(), midnight);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].tokens[0], 444);
    assert_eq!(recent(opts.clone(), midnight), expected);
    assert!(
        recent(opts, midnight + 2).is_empty(),
        "cutoff applies after selection"
    );
}

#[test]
#[serial_test::serial]
fn mixed_cache_hits_preserve_first_local_duplicate_and_subagent_identity() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let dir = home.path().join(".pi/agent/sessions/project");
    let fixture = |session: &str, input: i64| {
        [json!({"type":"session","id":session,"timestamp":"2026-02-10T00:00:00Z","cwd":"/tmp"}),
            json!({"type":"session_info","name":"subagent-reviewer-1a2b3c4d"}),
            json!({"type":"message","id":"entry","timestamp":"2026-02-10T00:00:01Z",
                "message":{"role":"assistant","responseId":"shared","provider":"openai",
                    "model":"gpt-4o","usage":{"input":input,"output":2,"cacheRead":3,"cacheWrite":4}}})]
            .iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n"
    };
    let first = dir.join("a.jsonl");
    write(&first, fixture("first", 11));
    write(&dir.join("b.jsonl"), fixture("second", 99));
    let opts = options(home.path(), &["pi"]);
    let expected = legacy(opts.clone(), 0);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].agent, "reviewer");
    assert_eq!(recent(opts.clone(), 0), expected);
    // Only the earlier source misses after this write. Emitting later cached
    // files before earlier misses would silently change the duplicate winner.
    write(&first, fixture("first", 12345));
    let expected = legacy(opts.clone(), 0);
    assert_eq!(expected[0].tokens[0], 12345);
    assert_eq!(recent(opts, 0), expected);
}

#[test]
#[serial_test::serial]
fn codex_turns_resume_and_timestamp_fallback_match_materialized_events() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let source = home.path().join(".codex/sessions/recent.jsonl");
    let first = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"session\"}}\n",
        "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-4o\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":100,\"cached_input_tokens\":20,\"output_tokens\":10,\"reasoning_output_tokens\":5}}}}\n"
    );
    write(&source, first);
    let opts = options(home.path(), &["codex"]);
    let expected = legacy(opts.clone(), 0);
    assert!(!expected.is_empty());
    assert!(expected.iter().all(|event| event.timestamp > 0));
    assert_eq!(recent(opts.clone(), 0), expected);
    let append = concat!(
        "{\"timestamp\":\"2040-01-01T00:00:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"type\":\"text\",\"text\":\"next\"}]}}}\n",
        "{\"timestamp\":\"2040-01-01T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":140,\"cached_input_tokens\":25,\"output_tokens\":20,\"reasoning_output_tokens\":8}}}}\n"
    );
    write(&source, first.to_string() + append);
    let expected = legacy(opts.clone(), 0);
    assert_eq!(recent(opts.clone(), 0), expected);
    assert_eq!(recent(opts.clone(), 0), expected);
    let expected_turns = syrtis_core::sessions::codex::parse_codex_file(&source)
        .iter()
        .filter(|message| message.is_turn_start)
        .count();
    assert_eq!(expected_turns, 2);
    let mut observed_turns = 0;
    visit_recent_local_messages(opts.clone(), 0, &mut |message| {
        observed_turns += usize::from(message.is_turn_start);
    })
    .unwrap();
    assert_eq!(observed_turns, expected_turns);
    // Truncation must take the existing full-parse fallback, not continue an
    // obsolete resume cursor or reuse its token deltas/turn count.
    write(&source, first);
    assert_eq!(recent(opts.clone(), 0), legacy(opts, 0));
}

#[test]
#[serial_test::serial]
fn recent_visitor_keeps_claude_retained_only_turns_after_compaction() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let source = home
        .path()
        .join(".claude/projects/project/conversation.jsonl");
    let row = |id: &str, input: i64| {
        json!({
        "type":"assistant", "timestamp":"2026-02-10T00:00:01Z", "requestId":id,
        "message":{"id":id,"model":"claude-sonnet-4","usage":{"input_tokens":input,"output_tokens":7,
            "cache_read_input_tokens":11,"cache_creation_input_tokens":13}}
    }).to_string()
    };
    let first = row("retained", 17);
    let live = row("live", 19);
    write(&source, first + "\n" + &live + "\n");
    let opts = options(home.path(), &["claude"]);
    let expected = legacy(opts.clone(), 0);
    assert_eq!(expected.len(), 2);
    assert_eq!(recent(opts.clone(), 0), expected);
    write(&source, live + "\n");
    assert_eq!(
        syrtis_core::sessions::claudecode::parse_claude_file_with_home(
            &source,
            Some(home.path())
        )
        .len(),
        1,
        "live source alone lost the compacted turn"
    );
    assert_eq!(recent(opts.clone(), 0), expected);
    assert_eq!(legacy(opts, 0), expected);
}

#[test]
#[serial_test::serial]
fn synthetic_hermes_selection_dedups_default_before_filtering_profiles() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    for (relative, rows) in [
        (".hermes/state.db", vec![("shared", "gpt-4o", 11)]),
        (
            ".hermes/profiles/gateway/state.db",
            vec![
                ("shared", "hf:org/Gateway-Test", 99),
                ("profile-only", "hf:org/Gateway-Test", 23),
            ],
        ),
    ] {
        let path = home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT,
             started_at REAL, message_count INTEGER, input_tokens INTEGER, output_tokens INTEGER,
             cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER,
             estimated_cost_usd REAL, actual_cost_usd REAL);",
        )
        .unwrap();
        for (id, model, input) in rows {
            db.execute(
                "INSERT INTO sessions VALUES (?1, ?2, NULL, 1770681601, 2, ?3, 3, 5, 7, 11, NULL, NULL)",
                rusqlite::params![id, model, input],
            ).unwrap();
        }
    }
    for (clients, inputs) in [
        (vec!["synthetic"], vec![23]),
        (vec!["synthetic", "hermes"], vec![11, 23]),
    ] {
        let opts = options(home.path(), &clients);
        let expected = legacy(opts.clone(), 0);
        let mut actual_inputs: Vec<_> = expected.iter().map(|event| event.tokens[0]).collect();
        actual_inputs.sort();
        assert_eq!(actual_inputs, inputs);
        assert_eq!(recent(opts.clone(), 0), expected);
        assert_eq!(recent(opts, 0), expected, "warm selection also agrees");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            clients: Some(vec!["synthetic".to_string()]),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        report.entries.iter().map(|entry| entry.input).sum::<i64>(),
        122,
        "report mode retains its client-filter-before-Hermes-dedup semantics",
    );
}

#[test]
#[serial_test::serial]
fn synthetic_scanning_does_not_enable_unrequested_claude_materialization() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    write(
        &home
            .path()
            .join(".claude/projects/project/conversation.jsonl"),
        json!({
            "type":"assistant", "timestamp":"2026-02-10T00:00:01Z", "requestId":"gateway",
            "message":{"id":"gateway","model":"hf:org/Gateway-Test",
                "usage":{"input_tokens":17,"output_tokens":7}}
        })
        .to_string()
            + "\n",
    );
    let mut gateway = opencode("gateway", 1770681601000, 31, 0.0);
    gateway["modelID"] = json!("hf:org/Gateway-Test");
    gateway["providerID"] = json!("unknown");
    write(
        &home
            .path()
            .join(".local/share/opencode/storage/message/session/gateway.json"),
        gateway.to_string(),
    );
    for (clients, claude_count) in [
        (vec!["synthetic"], 0),
        (vec!["synthetic", "opencode"], 0),
        (vec!["synthetic", "claude"], 1),
        (Vec::new(), 1),
    ] {
        let opts = options(home.path(), &clients);
        let expected = legacy(opts.clone(), 0);
        assert_eq!(
            expected
                .iter()
                .filter(|event| event.client == "claude")
                .count(),
            claude_count
        );
        assert!(expected.iter().any(|event| event.client == "opencode"));
        assert_eq!(recent(opts.clone(), 0), expected);
        assert_eq!(recent(opts, 0), expected);
    }
}

#[test]
#[serial_test::serial]
fn recent_opencode_batch_preserves_alias_authority_and_independent_identities() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let data = home.path().join(".local/share/opencode");
    std::fs::create_dir_all(&data).unwrap();
    let db = rusqlite::Connection::open(data.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);",
    )
    .unwrap();
    let timestamp = 1770681601000;
    let mut idless = opencode("unused", timestamp, 11, 0.0);
    idless.as_object_mut().unwrap().remove("id");
    db.execute(
        "INSERT INTO message VALUES ('a-alias', 'session', ?1)",
        [idless.to_string()],
    )
    .unwrap();
    for (row, id) in [("b-shared", "shared"), ("independent-row", "independent")] {
        db.execute(
            "INSERT INTO message VALUES (?1, 'session', ?2)",
            rusqlite::params![row, opencode(id, timestamp, 11, 0.0).to_string()],
        )
        .unwrap();
    }
    // The idless and embedded-ID SQLite copies collapse into one alias group.
    // JSON authority claims its fallback alias; the independent embedded ID
    // must survive even though every usage payload field is identical.
    write(
        &data.join("storage/message/session/authority.json"),
        opencode("a-alias", timestamp, 11, 0.25).to_string(),
    );
    let opts = options(home.path(), &["opencode"]);
    let expected = legacy(opts.clone(), timestamp);
    assert_eq!(expected.len(), 2);
    assert_eq!(recent(opts.clone(), timestamp), expected);
    let mut authorities = Vec::new();
    visit_recent_local_messages(opts.clone(), timestamp, &mut |message| {
        if message.cost_source == syrtis_core::CostSource::ProviderReported {
            authorities.push((message.dedup_key.clone(), message.cost));
        }
    })
    .unwrap();
    assert_eq!(authorities, vec![(Some("a-alias".to_string()), 0.25)]);
    let before = syrtis_core::local_source_change_token(&opts).unwrap();
    db.execute(
        "UPDATE message SET data = ?1 WHERE id = 'independent-row'",
        [opencode("independent", timestamp + 1, 12345, 0.0).to_string()],
    )
    .unwrap();
    assert_ne!(
        syrtis_core::local_source_change_token(&opts).unwrap(),
        before
    );
    let expected = legacy(opts.clone(), timestamp);
    assert_eq!(expected.len(), 2);
    assert!(expected.iter().any(|event| event.tokens[0] == 12345));
    assert_eq!(recent(opts, timestamp), expected);
}

#[test]
#[serial_test::serial]
fn recent_gjc_dedups_before_synthetic_client_filter() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let rows = [
        json!({"type":"session","id":"session","cwd":"/tmp"}),
        json!({"type":"message","id":"shared","message":{"role":"assistant","model":"gpt-4o",
            "provider":"openai","timestamp":1770681601000_i64,"usage":{"input":11,"output":2}}}),
        json!({"type":"message","id":"shared","message":{"role":"assistant","model":"hf:org/Gateway-Test",
            "provider":"unknown","timestamp":1770681601000_i64,"usage":{"input":99,"output":2}}}),
        json!({"type":"message","id":"unique","message":{"role":"assistant","model":"hf:org/Gateway-Test",
            "provider":"unknown","timestamp":1770681601000_i64,"usage":{"input":23,"output":2}}}),
    ].iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
    write(
        &home
            .path()
            .join(".gjc/agent/sessions/project/session.jsonl"),
        rows,
    );
    for (clients, inputs) in [
        (vec!["synthetic"], vec![23]),
        (vec!["synthetic", "gjc"], vec![11, 23]),
    ] {
        let opts = options(home.path(), &clients);
        let expected = legacy(opts.clone(), 0);
        let mut selected: Vec<_> = expected.iter().map(|event| event.tokens[0]).collect();
        selected.sort();
        assert_eq!(selected, inputs);
        assert_eq!(recent(opts, 0), expected);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            clients: Some(vec!["synthetic".to_string()]),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        report.entries.iter().map(|entry| entry.input).sum::<i64>(),
        122
    );
}

#[test]
#[serial_test::serial]
fn recent_hermes_dedups_empty_session_ids_like_materialized_usage() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    for (relative, input) in [
        (".hermes/state.db", 11),
        (".hermes/profiles/other/state.db", 99),
    ] {
        let path = home.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = rusqlite::Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT,
             started_at REAL, message_count INTEGER, input_tokens INTEGER, output_tokens INTEGER,
             cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER,
             estimated_cost_usd REAL, actual_cost_usd REAL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO sessions VALUES ('', 'gpt-4o', NULL, 1770681601, 1, ?1, 2, 0, 0, 0, NULL, NULL)",
            [input],
        ).unwrap();
    }
    let opts = options(home.path(), &["hermes"]);
    let expected = legacy(opts.clone(), 0);
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].tokens[0], 11);
    assert_eq!(recent(opts, 0), expected);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            clients: Some(vec!["hermes".to_string()]),
            ..Default::default()
        }))
        .unwrap();
    assert_eq!(
        report.entries.iter().map(|entry| entry.input).sum::<i64>(),
        110
    );
}

#[test]
#[serial_test::serial]
fn recent_claude_discovery_is_independent_of_aliased_client_roots() {
    let home = tempfile::TempDir::new().unwrap();
    let cache = tempfile::TempDir::new().unwrap();
    let _env = EnvGuard::isolate(home.path(), cache.path());
    let root = home.path().join(".pi/agent/sessions/project");
    write(&root.join("claude.jsonl"), json!({
        "type":"assistant","timestamp":"2026-02-10T00:00:01Z","requestId":"aliased",
        "message":{"id":"aliased","model":"claude-sonnet-4","usage":{"input_tokens":17,"output_tokens":7}}
    }).to_string() + "\n");
    for clients in [
        vec!["claude", "pi"],
        vec!["claude", "pi", "synthetic"],
        Vec::new(),
    ] {
        let mut opts = options(home.path(), &clients);
        opts.scanner_settings
            .extra_scan_paths
            .insert("claude".to_string(), vec![root.clone()]);
        let expected = legacy(opts.clone(), 0);
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].client, "claude");
        assert_eq!(recent(opts.clone(), 0), expected);
        assert_eq!(recent(opts, 0), expected);
    }
    let mut settings = syrtis_core::scanner::ScannerSettings::default();
    settings
        .extra_scan_paths
        .insert("claude".to_string(), vec![root]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let report = runtime
        .block_on(get_hourly_report(ReportOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            clients: Some(vec!["claude".to_string(), "pi".to_string()]),
            scanner_settings: settings,
            ..Default::default()
        }))
        .unwrap();
    assert!(
        report.entries.is_empty(),
        "report retains union-discovery ownership"
    );
}
