use super::UnifiedMessage;
use crate::{pricing, provider_identity, TokenBreakdown};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub fn parse_antigravity_file(path: &Path) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    match visit_antigravity_file(path, &mut |message| {
        messages.push(message);
        Ok(())
    }) {
        Ok(()) => messages,
        Err(_) => Vec::new(),
    }
}

pub(crate) fn visit_antigravity_file(
    path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let file = super::utils::open_source_file(path).map_err(|error| error.to_string())?;
    visit_antigravity_reader(BufReader::new(file), sink)
}

fn visit_antigravity_reader(
    mut reader: impl BufRead,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let mut session_model: Option<String> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        let row_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match row_type {
            "session_meta" => {
                if let Some(model_id) = value
                    .get("modelId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                {
                    session_model = Some(model_id.to_string());
                }
            }
            "usage" => {
                if let Some(message) = parse_usage_row(&value, session_model.as_deref()) {
                    sink(message)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn parse_usage_row(value: &Value, fallback_model: Option<&str>) -> Option<UnifiedMessage> {
    let session_id = value.get("sessionId").and_then(Value::as_str)?.to_string();
    let timestamp = to_safe_i64(value.get("timestamp"));
    if timestamp <= 0 {
        return None;
    }

    let model_id = value
        .get("modelId")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.to_string())
        .or_else(|| fallback_model.map(|text| text.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    let model_id = pricing::aliases::resolve_alias(&model_id)
        .unwrap_or(model_id.as_str())
        .to_string();

    let provider_id = value
        .get("providerId")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.to_string())
        .unwrap_or_else(|| infer_provider(&model_id).to_string());
    let provider_id = provider_identity::canonical_provider(&provider_id).unwrap_or(provider_id);

    let input = to_safe_i64(value.get("input"));
    let output = to_safe_i64(value.get("output"));
    let cache_read = to_safe_i64(value.get("cacheRead"));
    let cache_write = to_safe_i64(value.get("cacheWrite"));
    let reasoning = to_safe_i64(value.get("reasoning"));
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 && reasoning == 0 {
        return None;
    }

    let dedup_key = value
        .get("responseId")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.to_string());

    Some(UnifiedMessage::new_with_dedup(
        "antigravity",
        model_id,
        provider_id,
        session_id,
        timestamp,
        TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            cache_write_1h: 0,
        },
        0.0,
        dedup_key,
    ))
}

fn infer_provider(model: &str) -> &'static str {
    provider_identity::inferred_provider_from_model(model).unwrap_or("antigravity")
}

fn to_safe_i64(value: Option<&Value>) -> i64 {
    value
        .and_then(|inner| {
            inner
                .as_i64()
                .or_else(|| inner.as_u64().and_then(|number| i64::try_from(number).ok()))
                .or_else(|| inner.as_str().and_then(|text| text.parse::<i64>().ok()))
        })
        .unwrap_or(0)
        .max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visitor_preserves_model_transitions_and_explicit_precedence() {
        let input = concat!(
            "{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":1,\"input\":1}\n",
            "{\"type\":\"session_meta\",\"modelId\":\"claude-sonnet-4.6\"}\n",
            "malformed json\n",
            "{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":2,\"input\":2,\"responseId\":\"r\"}\n",
            "{\"type\":\"session_meta\",\"modelId\":\" \"}\n",
            "{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":3,\"input\":3,\"modelId\":\"gemini-3.1-pro\",\"providerId\":\"google\"}\n",
            "{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":4,\"input\":4}\n",
            "{\"type\":\"session_meta\",\"modelId\":\"gemini-3.1-pro\"}\n",
            "{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":5,\"input\":5}\n",
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), input).unwrap();
        let messages = parse_antigravity_file(file.path());
        assert_eq!(messages.len(), 5);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.model_id.as_str())
                .collect::<Vec<_>>(),
            [
                "unknown",
                "claude-sonnet-4-6",
                "gemini-3.1-pro",
                "claude-sonnet-4-6",
                "gemini-3.1-pro"
            ]
        );
        assert_eq!(messages[1].dedup_key.as_deref(), Some("r"));
        assert_eq!(messages[1].provider_id, "anthropic");
        assert_eq!(messages[2].provider_id, "google");
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn visitor_propagates_read_utf8_and_sink_failures() {
        use std::io::{self, Cursor, Read};

        struct ReadFailure;
        impl Read for ReadFailure {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }

        let row = b"{\"type\":\"usage\",\"sessionId\":\"s\",\"timestamp\":1,\"input\":1}\n";
        let mut emitted = 0;
        let result = visit_antigravity_reader(
            BufReader::new(Cursor::new(row).chain(ReadFailure)),
            &mut |_| {
                emitted += 1;
                Ok(())
            },
        );
        assert!(result.unwrap_err().contains("injected read failure"));
        assert_eq!(emitted, 1);

        let file = tempfile::NamedTempFile::new().unwrap();
        let mut invalid = row.to_vec();
        invalid.extend_from_slice(b"\xff\n");
        std::fs::write(file.path(), invalid).unwrap();
        assert!(visit_antigravity_file(file.path(), &mut |_| Ok(())).is_err());
        assert!(parse_antigravity_file(file.path()).is_empty());

        std::fs::write(file.path(), [row.as_slice(), row.as_slice()].concat()).unwrap();
        let mut calls = 0;
        let error = visit_antigravity_file(file.path(), &mut |_| {
            calls += 1;
            Err("sink rejected".to_string())
        })
        .unwrap_err();
        assert_eq!(error, "sink rejected");
        assert_eq!(calls, 1);
    }

    #[test]
    fn parse_usage_row_with_meta_fallback() {
        let input = r#"{"type":"session_meta","sessionId":"abc","modelId":"claude-sonnet-4.6"}
{"type":"usage","sessionId":"abc","timestamp":1711200000000,"input":12,"output":4,"cacheRead":2,"cacheWrite":0,"reasoning":1,"responseId":"resp-1"}
"#;

        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), input).unwrap();

        let messages = parse_antigravity_file(path.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "antigravity");
        assert_eq!(messages[0].model_id, "claude-sonnet-4-6");
        assert_eq!(messages[0].tokens.input, 12);
        assert_eq!(messages[0].tokens.reasoning, 1);
        assert_eq!(messages[0].dedup_key.as_deref(), Some("resp-1"));
    }

    #[test]
    fn parse_usage_row_resolves_placeholder_model_alias() {
        let input = r#"{"type":"usage","sessionId":"abc","modelId":"MODEL_PLACEHOLDER_M26","timestamp":1711200000000,"input":12,"output":4,"cacheRead":2,"cacheWrite":0,"reasoning":1}
"#;

        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), input).unwrap();

        let messages = parse_antigravity_file(path.path());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-opus-4-6");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn parse_usage_row_resolves_current_placeholder_models() {
        let input = r#"{"type":"usage","sessionId":"abc","modelId":"model_placeholder_m84","timestamp":1711200000000,"input":12,"output":4,"cacheRead":2,"cacheWrite":0,"reasoning":1}
{"type":"usage","sessionId":"abc","modelId":"model_placeholder_m16","timestamp":1711200000001,"input":8,"output":3,"cacheRead":0,"cacheWrite":0,"reasoning":0}
"#;

        let path = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(path.path(), input).unwrap();

        let messages = parse_antigravity_file(path.path());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model_id, "gemini-3-flash-preview");
        assert_eq!(messages[0].provider_id, "google");
        assert_eq!(messages[1].model_id, "gemini-3.1-pro");
        assert_eq!(messages[1].provider_id, "google");
    }
}
