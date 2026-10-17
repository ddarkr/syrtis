//! Roo Code task parser
//!
//! Parses task-based logs from VS Code globalStorage directories:
//! - tasks/<taskId>/ui_messages.json
//! - tasks/<taskId>/api_conversation_history.json

use super::utils::{
    extract_i64, open_optional_source_file, open_source_file, parse_timestamp_str,
    visit_simd_json_array,
};
use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct UiMessageEntry {
    #[serde(rename = "type")]
    entry_type: Option<String>,
    say: Option<String>,
    text: Option<String>,
    ts: Option<Value>,
    #[serde(rename = "modelInfo")]
    model_info: Option<UiModelInfo>,
}

/// Per-message model identity, written by current Cline on every
/// `ui_messages.json` entry as `modelInfo` (upstream #1340, issue #1321).
///
/// Cline 4.x no longer writes the `<model>` tag inside
/// `<environment_details>` blocks nor the `apiProtocol` field of the
/// `api_req_started` payload, so those file-level heuristics resolve
/// `unknown/unknown` for every current task. When an entry carries
/// `modelInfo`, it is the authoritative identity for that message; older
/// Roo-style records keep the legacy heuristics.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UiModelInfo {
    provider_id: Option<String>,
    model_id: Option<String>,
}

pub fn parse_roocode_file(path: &Path) -> Vec<UnifiedMessage> {
    parse_roo_kilo_file(path, "roocode")
}

pub(crate) fn visit_roocode_file(
    path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    visit_roo_kilo_file(path, "roocode", sink)
}

pub(crate) fn parse_roo_kilo_file(path: &Path, source: &str) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    if visit_roo_kilo_file(path, source, &mut |message| {
        messages.push(message);
        Ok(())
    })
    .is_err()
    {
        messages.clear();
    }
    messages
}

pub(crate) fn visit_roo_kilo_file(
    path: &Path,
    source: &str,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let reader = open_source_file(path).map_err(|error| error.to_string())?;
    let session_id = extract_session_id(path);
    let (model_id, agent) = read_task_metadata(path)?;
    visit_simd_json_array(
        std::io::BufReader::new(reader),
        &mut |entry: UiMessageEntry| {
            if entry.entry_type.as_deref() != Some("say")
                || entry.say.as_deref() != Some("api_req_started")
            {
                return Ok(());
            }

            let text = match entry.text {
                Some(t) => t,
                None => return Ok(()),
            };

            let timestamp = match parse_entry_timestamp(entry.ts.as_ref()) {
                Some(ts) => ts,
                None => return Ok(()),
            };

            let payload = match parse_api_req_started_payload(text) {
                Some(p) => p,
                None => return Ok(()),
            };

            // `modelInfo` is per-message and states the model that actually
            // answered this request; the file-level `<model>`-tag heuristic
            // labels every row in a task with the last tag it saw, so the
            // per-entry identity wins whenever it is present.
            let model = entry
                .model_info
                .as_ref()
                .and_then(|info| info.model_id.clone())
                .map(|model| model.trim().to_string())
                .filter(|model| !model.is_empty())
                .unwrap_or_else(|| model_id.clone());
            // Provider is the other way around: a nested `apiProtocol`
            // ("bedrock/anthropic") carries reseller routing that the bare
            // `modelInfo.providerId` would flatten to its last segment, so the
            // legacy field keeps precedence whenever it says anything, and
            // `modelInfo` fills the silence current Cline leaves.
            let provider = match provider_from_api_protocol(payload.api_protocol.as_deref()) {
                protocol if protocol != "unknown" => protocol,
                _ => entry
                    .model_info
                    .as_ref()
                    .and_then(|info| info.provider_id.clone())
                    .map(|provider| provider.trim().to_string())
                    .filter(|provider| !provider.is_empty())
                    .unwrap_or_else(|| "unknown".to_string()),
            };

            sink(UnifiedMessage::new_with_agent(
                source,
                model,
                provider,
                session_id.clone(),
                timestamp,
                TokenBreakdown {
                    input: payload.tokens_in,
                    output: payload.tokens_out,
                    cache_read: payload.cache_reads,
                    cache_write: payload.cache_writes,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                payload.cost,
                agent.clone(),
            ))
        },
    )
}

fn extract_session_id(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn read_task_metadata(ui_messages_path: &Path) -> Result<(String, Option<String>), String> {
    let history_path = history_path_for_ui_messages(ui_messages_path);
    match open_optional_source_file(&history_path) {
        Ok(reader) => scan_task_metadata(reader).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(("unknown".to_string(), None))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Keep only tag values and a delimiter-sized suffix, never history prose.
fn scan_task_metadata(mut reader: impl std::io::Read) -> std::io::Result<(String, Option<String>)> {
    const START: &[u8] = b"<environment_details>";
    const END: &[u8] = b"</environment_details>";
    struct Tag {
        open: &'static [u8],
        close: &'static [u8],
        started: bool,
        done: bool,
        value: Vec<u8>,
    }
    impl Tag {
        fn new(open: &'static [u8], close: &'static [u8]) -> Self {
            Self {
                open,
                close,
                started: false,
                done: false,
                value: Vec::new(),
            }
        }
        fn push(&mut self, byte: u8, suffix: &[u8]) {
            if self.done {
                return;
            }
            if self.started {
                self.value.push(byte);
                if self.value.ends_with(self.close) {
                    self.value.truncate(self.value.len() - self.close.len());
                    self.done = true;
                }
            } else if suffix.ends_with(self.open) {
                self.started = true;
            }
        }
        fn take(&mut self) -> Option<String> {
            if !self.done {
                return None;
            }
            let value = String::from_utf8(std::mem::take(&mut self.value)).ok()?;
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
    }
    let tags = || {
        [
            Tag::new(b"<model>", b"</model>"),
            Tag::new(b"<slug>", b"</slug>"),
            Tag::new(b"<name>", b"</name>"),
        ]
    };
    let mut current = tags();
    let mut last: [Option<String>; 3] = [None, None, None];
    let mut inside = false;
    let mut suffix = Vec::with_capacity(END.len());
    let mut buffer = [0u8; 8192];
    let mut utf8_tail = Vec::with_capacity(4);
    let mut chunk = Vec::with_capacity(8196);
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        // Validate UTF-8 even in discarded prose, matching read_to_string.
        chunk.clear();
        chunk.extend_from_slice(&utf8_tail);
        utf8_tail.clear();
        chunk.extend_from_slice(&buffer[..count]);
        match std::str::from_utf8(&chunk) {
            Ok(_) => {}
            Err(error) if error.error_len().is_none() => {
                utf8_tail.extend_from_slice(&chunk[error.valid_up_to()..]);
            }
            Err(_) => return Ok(("unknown".to_string(), None)),
        }
        for &byte in &buffer[..count] {
            if suffix.len() == END.len() {
                suffix.remove(0);
            }
            suffix.push(byte);
            if !inside {
                if suffix.ends_with(START) {
                    inside = true;
                    current = tags();
                    suffix.clear();
                }
            } else if suffix.ends_with(END) {
                for (last, tag) in last.iter_mut().zip(&mut current) {
                    if let Some(value) = tag.take() {
                        *last = Some(value);
                    }
                }
                inside = false;
                suffix.clear();
            } else {
                for tag in &mut current {
                    tag.push(byte, &suffix);
                }
            }
        }
    }
    if !utf8_tail.is_empty() {
        return Ok(("unknown".to_string(), None));
    }
    let [model, slug, name] = last;
    Ok((
        model.unwrap_or_else(|| "unknown".to_string()),
        slug.or(name),
    ))
}

pub(crate) fn history_path_for_ui_messages(ui_messages_path: &Path) -> PathBuf {
    ui_messages_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("api_conversation_history.json")
}

#[cfg(test)]
fn extract_model_and_agent(content: &str) -> (String, Option<String>) {
    const ENV_START: &str = "<environment_details>";
    const ENV_END: &str = "</environment_details>";

    let mut offset = 0usize;
    let mut last_model: Option<String> = None;
    let mut last_slug: Option<String> = None;
    let mut last_name: Option<String> = None;

    while let Some(start_rel) = content[offset..].find(ENV_START) {
        let start_idx = offset + start_rel + ENV_START.len();
        let rest = &content[start_idx..];

        let Some(end_rel) = rest.find(ENV_END) else {
            break;
        };
        let end_idx = start_idx + end_rel;
        let block = &content[start_idx..end_idx];

        if let Some(model) = extract_tag_value(block, "model") {
            last_model = Some(model);
        }
        if let Some(slug) = extract_tag_value(block, "slug") {
            last_slug = Some(slug);
        }
        if let Some(name) = extract_tag_value(block, "name") {
            last_name = Some(name);
        }

        offset = end_idx + ENV_END.len();
    }

    let model = last_model.unwrap_or_else(|| "unknown".to_string());
    let agent = last_slug.or(last_name);
    (model, agent)
}

#[cfg(test)]
fn extract_tag_value(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);

    let start_idx = block.find(&open)? + open.len();
    let rest = &block[start_idx..];
    let end_rel = rest.find(&close)?;
    let value = rest[..end_rel].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_entry_timestamp(ts: Option<&Value>) -> Option<i64> {
    let value = ts?;
    let ts_str = if let Some(s) = value.as_str() {
        s.to_string()
    } else if let Some(i) = value.as_i64() {
        i.to_string()
    } else {
        value.as_u64()?.to_string()
    };

    parse_timestamp_str(&ts_str)
}

struct ApiReqStartedPayload {
    cost: f64,
    tokens_in: i64,
    tokens_out: i64,
    cache_reads: i64,
    cache_writes: i64,
    api_protocol: Option<String>,
}

fn parse_api_req_started_payload(text: String) -> Option<ApiReqStartedPayload> {
    let mut bytes = text.into_bytes();
    let value: Value = simd_json::from_slice(&mut bytes).ok()?;

    let cost = extract_f64(value.get("cost")).unwrap_or(0.0).max(0.0);
    let tokens_in = extract_i64(value.get("tokensIn")).unwrap_or(0).max(0);
    let tokens_out = extract_i64(value.get("tokensOut")).unwrap_or(0).max(0);
    let cache_reads = extract_i64(value.get("cacheReads")).unwrap_or(0).max(0);
    let cache_writes = extract_i64(value.get("cacheWrites")).unwrap_or(0).max(0);
    let api_protocol = value
        .get("apiProtocol")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Some(ApiReqStartedPayload {
        cost,
        tokens_in,
        tokens_out,
        cache_reads,
        cache_writes,
        api_protocol,
    })
}

fn extract_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|val| {
        val.as_f64()
            .or_else(|| val.as_i64().map(|v| v as f64))
            .or_else(|| val.as_u64().map(|v| v as f64))
            .or_else(|| val.as_str().and_then(|s| s.parse::<f64>().ok()))
    })
}

fn provider_from_api_protocol(api_protocol: Option<&str>) -> String {
    api_protocol
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_task(
        dir: &TempDir,
        task_id: &str,
        ui_messages_content: &str,
        history_content: Option<&str>,
    ) -> PathBuf {
        let task_dir = dir.path().join("tasks").join(task_id);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("ui_messages.json"), ui_messages_content).unwrap();
        if let Some(history) = history_content {
            fs::write(task_dir.join("api_conversation_history.json"), history).unwrap();
        }
        task_dir.join("ui_messages.json")
    }

    #[test]
    fn test_parse_roocode_valid_api_req_started() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "{\"cost\":0.12,\"tokensIn\":100,\"tokensOut\":50,\"cacheReads\":20,\"cacheWrites\":5,\"apiProtocol\":\"anthropic\"}"
  },
  {
    "type": "say",
    "say": "assistant_message",
    "ts": "2026-02-18T12:00:01Z",
    "text": "{}"
  }
]"#;
        let history = r#"before
<environment_details>
<model>claude-sonnet-4</model>
<slug>architect</slug>
<name>Architect</name>
</environment_details>
after"#;
        let path = setup_task(&dir, "task-abc", ui_messages, Some(history));

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "roocode");
        assert_eq!(messages[0].model_id, "claude-sonnet-4");
        assert_eq!(messages[0].provider_id, "anthropic");
        assert_eq!(messages[0].session_id, "task-abc");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.cache_write, 5);
        assert_eq!(messages[0].cost, 0.12);
        assert_eq!(messages[0].agent.as_deref(), Some("architect"));
    }

    // Ported from upstream #1340.
    /// Cline 4.x writes neither the `<model>` tag nor `apiProtocol`; it states
    /// model identity per message through `modelInfo` (#1321). Without the
    /// per-message preference every such task resolved to `unknown/unknown`
    /// and priced at $0.
    #[test]
    fn test_parse_roocode_prefers_model_info_over_legacy_fields() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "ts": 1789022952376,
    "type": "say",
    "say": "task",
    "text": "666",
    "modelInfo": {"providerId": "anthropic", "modelId": "claude-sonnet-5", "mode": "act"}
  },
  {
    "ts": 1789022955199,
    "type": "say",
    "say": "api_req_started",
    "text": "{\"request\":\"<task>666</task>\",\"tokensIn\":3638,\"tokensOut\":409,\"cacheWrites\":0,\"cacheReads\":0,\"cost\":0.011366}",
    "modelInfo": {"providerId": "anthropic", "modelId": "claude-sonnet-5", "mode": "act"}
  }
]"#;
        // A history file exists but carries no <model> tag, matching current
        // Cline: the environment_details block is present, the model is not.
        let history = r#"before
<environment_details>
<slug>act</slug>
</environment_details>
after"#;
        let path = setup_task(&dir, "task-cline4", ui_messages, Some(history));

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-5");
        assert_eq!(messages[0].provider_id, "anthropic");
        assert_eq!(messages[0].tokens.input, 3638);
        assert_eq!(messages[0].tokens.output, 409);
        assert_eq!(messages[0].cost, 0.011366);
    }

    /// A task that switched models mid-task: the per-message `modelInfo`
    /// labels each row with the model that answered it, where the file-level
    /// heuristic stamped every row with the last `<model>` tag.
    #[test]
    fn test_parse_roocode_model_info_labels_each_message() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "{\"cost\":0.1,\"tokensIn\":10,\"tokensOut\":1,\"apiProtocol\":\"openai\"}",
    "modelInfo": {"providerId": "openai", "modelId": "gpt-5.1"}
  },
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:05:00Z",
    "text": "{\"cost\":0.2,\"tokensIn\":20,\"tokensOut\":2}",
    "modelInfo": {"providerId": "anthropic", "modelId": "claude-sonnet-5"}
  }
]"#;
        let path = setup_task(&dir, "task-switch", ui_messages, None);

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model_id, "gpt-5.1");
        assert_eq!(messages[0].provider_id, "openai");
        assert_eq!(messages[1].model_id, "claude-sonnet-5");
        // The second entry carries no `apiProtocol`, so its `modelInfo`
        // provider fills in.
        assert_eq!(messages[1].provider_id, "anthropic");
    }

    /// A nested `apiProtocol` routes through a reseller; the bare
    /// `modelInfo.providerId` must not flatten it away.
    #[test]
    fn test_parse_roocode_nested_api_protocol_outranks_model_info_provider() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "{\"cost\":0.1,\"tokensIn\":10,\"tokensOut\":1,\"apiProtocol\":\"bedrock/anthropic\"}",
    "modelInfo": {"providerId": "anthropic", "modelId": "claude-sonnet-5"}
  }
]"#;
        let path = setup_task(&dir, "task-nested", ui_messages, None);

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "bedrock/anthropic");
        assert_eq!(messages[0].model_id, "claude-sonnet-5");
    }

    /// Blank `modelInfo` fields must not blank out a working legacy
    /// resolution; empty strings fall through to the file-level heuristic.
    #[test]
    fn test_parse_roocode_blank_model_info_falls_back_to_legacy() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "{\"cost\":0.12,\"tokensIn\":100,\"tokensOut\":50,\"apiProtocol\":\"anthropic\"}",
    "modelInfo": {"providerId": "", "modelId": "  "}
  }
]"#;
        let history = r#"
<environment_details>
<model>claude-sonnet-4</model>
</environment_details>
"#;
        let path = setup_task(&dir, "task-blank", ui_messages, Some(history));

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn test_parse_roocode_skips_malformed_payload_entry() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "not-json"
  },
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:02Z",
    "text": "{\"cost\":0.03,\"tokensIn\":10,\"tokensOut\":2,\"cacheReads\":1,\"cacheWrites\":0,\"apiProtocol\":\"openai\"}"
  }
]"#;
        let path = setup_task(&dir, "task-def", ui_messages, None);

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "openai");
        assert_eq!(messages[0].model_id, "unknown");
        assert_eq!(messages[0].agent, None);
    }

    #[test]
    fn test_parse_roocode_preserves_nested_reseller_api_protocol() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "2026-02-18T12:00:00Z",
    "text": "{\"cost\":0.12,\"tokensIn\":100,\"tokensOut\":50,\"cacheReads\":20,\"cacheWrites\":5,\"apiProtocol\":\"bedrock/anthropic\"}"
  }
]"#;
        let history = r#"before
<environment_details>
<model>claude-sonnet-4</model>
</environment_details>
after"#;
        let path = setup_task(&dir, "task-nested-provider", ui_messages, Some(history));

        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "bedrock/anthropic");
    }

    #[test]
    fn test_parse_roocode_skips_invalid_timestamp() {
        let dir = TempDir::new().unwrap();
        let ui_messages = r#"[
  {
    "type": "say",
    "say": "api_req_started",
    "ts": "not-a-time",
    "text": "{\"cost\":0.12,\"tokensIn\":100,\"tokensOut\":50,\"cacheReads\":20,\"cacheWrites\":5,\"apiProtocol\":\"anthropic\"}"
  }
]"#;
        let path = setup_task(&dir, "task-time", ui_messages, None);

        let messages = parse_roocode_file(&path);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_roocode_invalid_file_json_is_ignored() {
        let dir = TempDir::new().unwrap();
        let path = setup_task(&dir, "task-invalid", "{not-json", None);

        let messages = parse_roocode_file(&path);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_extract_model_and_agent_prefers_slug_then_name() {
        let content = r#"
<environment_details>
<model>gpt-5</model>
<name>Builder</name>
</environment_details>
<environment_details>
<model>gpt-5.1</model>
<slug>reviewer</slug>
<name>Reviewer</name>
</environment_details>
"#;

        let (model, agent) = extract_model_and_agent(content);
        assert_eq!(model, "gpt-5.1");
        assert_eq!(agent.as_deref(), Some("reviewer"));
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::io::{self, Read};

    struct Chunks<'a> {
        bytes: &'a [u8],
        size: usize,
    }
    impl Read for Chunks<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let count = output.len().min(self.size).min(self.bytes.len());
            output[..count].copy_from_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            Ok(count)
        }
    }

    #[test]
    fn metadata_boundaries_match_legacy_precedence() {
        let content = format!(
            "{}<environment_details><model> α </model><slug>old</slug><name>first</name></environment_details>\
             <environment_details><model> </model><model>ignored</model><name>new</name></environment_details>\
             <environment_details><model>latest</model><slug>final</slug></environment_details>\
             <environment_details><model>unclosed</model>{}",
            "x".repeat(20000), "y".repeat(20000)
        );
        let expected = extract_model_and_agent(&content);
        for size in [1, 2, 7, 21, 22, 8192] {
            assert_eq!(
                scan_task_metadata(Chunks {
                    bytes: content.as_bytes(),
                    size
                })
                .unwrap(),
                expected
            );
        }
        assert_eq!(expected, ("latest".into(), Some("final".into())));
        assert_eq!(
            scan_task_metadata(&b"\xff"[..]).unwrap(),
            ("unknown".into(), None)
        );
    }

    #[test]
    fn metadata_retries_interrupted_reads() {
        struct InterruptedOnce<'a> {
            bytes: &'a [u8],
            interrupted: bool,
        }
        impl Read for InterruptedOnce<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.bytes.read(output)
            }
        }
        let text = "<environment_details><model>λ</model><slug>agent</slug></environment_details>";
        assert_eq!(
            scan_task_metadata(InterruptedOnce {
                bytes: text.as_bytes(),
                interrupted: false
            })
            .unwrap(),
            extract_model_and_agent(text)
        );
    }

    #[test]
    fn visitor_keeps_simd_inner_payload_cost_bits() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ui_messages.json");
        let entry = r#"{"type":"say","say":"api_req_started","ts":1771416000000,"text":"{\"cost\":51.248178375505404,\"tokensIn\":3}"}"#;
        let mut payload = br#"{"cost":51.248178375505404,"tokensIn":3}"#.to_vec();
        let baseline: Value = simd_json::from_slice(&mut payload).unwrap();
        let expected = baseline["cost"].as_f64().unwrap().to_bits();
        assert_eq!(expected, 51.248178375505404_f64.to_bits());
        std::fs::write(&path, format!("[{entry}]")).unwrap();
        let messages = parse_roocode_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost.to_bits(), expected);
    }

    #[test]
    fn visitor_errors_abort_collectors_and_propagate_sink_failure() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ui_messages.json");
        let entry = r#"{"type":"say","say":"api_req_started","ts":1771416000000,"modelInfo":{"modelId":"actual","providerId":"anthropic"},"text":"{\"tokensIn\":3,\"apiProtocol\":\"bedrock/anthropic\"}"}"#;
        std::fs::write(&path, format!("[{entry},{{\"ignored\":1e400}}]")).unwrap();
        assert!(visit_roocode_file(&path, &mut |_| Ok(())).is_err());
        assert!(parse_roocode_file(&path).is_empty());
        std::fs::write(&path, format!("[{entry},")).unwrap();
        let mut count = 0;
        assert!(visit_roocode_file(&path, &mut |_| {
            count += 1;
            Ok(())
        })
        .is_err());
        assert_eq!(count, 1);
        assert!(parse_roocode_file(&path).is_empty());
        std::fs::write(&path, format!("[{entry}] true")).unwrap();
        assert!(parse_roocode_file(&path).is_empty());
        std::fs::write(&path, format!("[{entry},{entry}]")).unwrap();
        let error = visit_roocode_file(&path, &mut |message| {
            assert_eq!(message.model_id, "actual");
            assert_eq!(message.provider_id, "bedrock/anthropic");
            Err("sink stopped".into())
        })
        .unwrap_err();
        assert!(error.contains("sink stopped"));
        assert_eq!(parse_roocode_file(&path).len(), 2);
        let valid = format!("[{entry},{{\"ignored\":\"λ😀\"}}]");
        std::fs::write(&path, &valid).unwrap();
        assert_eq!(parse_roocode_file(&path).len(), 1);
        let mut invalid = valid.into_bytes();
        let index = invalid.iter().position(|byte| *byte == 0xce).unwrap();
        invalid[index] = 0xff;
        std::fs::write(&path, invalid).unwrap();
        assert!(visit_roocode_file(&path, &mut |_| Ok(())).is_err());
        assert!(parse_roocode_file(&path).is_empty());
    }
}
