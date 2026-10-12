//! OpenClaw session parser
//!
//! Parses OpenClaw transcript JSONL files from agent directories.
//! Supports legacy sessions.json index parsing for compatibility.

use super::utils::read_file_or_none;
use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

// Archived transcripts are immutable zstd files. Decoded output is bounded
// while streaming so a corrupt archive cannot expand without limit.
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// Stream one OpenClaw transcript to `sink`. Plain files stream line by line;
/// `.zst` archives stream decoded lines through the same lossy framing, with
/// the decoded byte count enforced while decoding. A corrupt, truncated, or
/// oversize archive yields `Err` with nothing committed, so transactional
/// ingestion publishes nothing partial. A transcript without filename
/// identity is empty success, as before. `window_log_max(26)` keeps the
/// decoder window bounded as before.
pub(crate) fn visit_openclaw_transcript(
    transcript_path: &Path,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let Some(session_id) = openclaw_session_id(transcript_path) else {
        return Ok(());
    };
    visit_openclaw_session(transcript_path, &session_id, sink)
}

fn openclaw_session_id(transcript_path: &Path) -> Option<String> {
    transcript_path
        .file_name()
        .and_then(|name| {
            name.to_string_lossy()
                .split_once(".jsonl")
                .map(|(id, _)| id.to_string())
        })
        .filter(|id| !id.is_empty())
}

#[derive(Debug, Deserialize)]
struct SessionIndex {
    #[serde(flatten)]
    sessions: HashMap<String, SessionEntry>,
}

#[derive(Debug, Deserialize)]
struct SessionEntry {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "sessionFile")]
    session_file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenClawEntry {
    #[serde(rename = "type")]
    entry_type: String,
    message: Option<OpenClawMessage>,
    #[serde(rename = "customType")]
    custom_type: Option<String>,
    data: Option<OpenClawModelData>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    provider: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenClawMessage {
    role: Option<String>,
    usage: Option<OpenClawUsage>,
    timestamp: Option<i64>,
    provider: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenClawModelData {
    provider: Option<String>,
    #[serde(rename = "modelId")]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenClawUsage {
    input: Option<i64>,
    output: Option<i64>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<i64>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<i64>,
    #[serde(rename = "totalTokens")]
    #[allow(dead_code)]
    total_tokens: Option<i64>,
    cost: Option<OpenClawCost>,
}

#[derive(Debug, Deserialize)]
struct OpenClawCost {
    total: Option<f64>,
}

pub fn parse_openclaw_index(index_path: &Path) -> Vec<UnifiedMessage> {
    let Some(data) = read_file_or_none(index_path) else {
        return Vec::new();
    };

    let mut bytes = data;
    let index: SessionIndex = match simd_json::from_slice(&mut bytes) {
        Ok(i) => i,
        Err(_) => return Vec::new(),
    };

    let mut all_messages = Vec::new();
    let index_dir = index_path.parent().unwrap_or_else(|| Path::new("."));

    for (_key, entry) in index.sessions {
        let session_path = resolve_session_path(index_dir, &entry);
        if super::utils::optional_source_exists(&session_path) {
            let messages = parse_openclaw_session(&session_path, &entry.session_id);
            all_messages.extend(messages);
        }
    }

    all_messages
}

pub fn parse_openclaw_transcript(transcript_path: &Path) -> Vec<UnifiedMessage> {
    let Some(session_id) = openclaw_session_id(transcript_path) else {
        return Vec::new();
    };
    parse_openclaw_session(transcript_path, &session_id)
}

/// Collect one session through the streaming visitor. A corrupt, truncated,
/// or oversize archive discards its whole prefix so compatible callers never
/// report partial success; the index caller keeps its skip semantics by
/// skipping empty results as before.
fn parse_openclaw_session(session_path: &Path, session_id: &str) -> Vec<UnifiedMessage> {
    let mut messages = Vec::new();
    match visit_openclaw_session(session_path, session_id, &mut |message| {
        messages.push(message);
        Ok(())
    }) {
        Ok(()) => messages,
        Err(_) => Vec::new(),
    }
}

fn resolve_session_path(index_dir: &Path, entry: &SessionEntry) -> PathBuf {
    match entry
        .session_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(session_file) => {
            let path = Path::new(session_file);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                index_dir.join(path)
            }
        }
        None => index_dir.join(format!("{}.jsonl", entry.session_id)),
    }
}

/// Shared OpenClaw line driver: `Ok(())` on success, `Err` when the sink
/// rejects a message or a `.zst` archive is corrupt/truncated/oversize.
pub(crate) fn visit_openclaw_session(
    session_path: &Path,
    session_id: &str,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let file_mtime_ms = super::utils::source_metadata(session_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut current_model: Option<String> = None;
    let mut current_provider: Option<String> = None;
    let mut buffer = Vec::with_capacity(4096);
    if session_path
        .extension()
        .is_some_and(|extension| extension == "zst")
    {
        visit_openclaw_archive_lines(
            session_path,
            session_id,
            file_mtime_ms,
            &mut current_model,
            &mut current_provider,
            &mut buffer,
            sink,
        )
    } else {
        let file =
            super::utils::open_source_file(session_path).map_err(|error| error.to_string())?;
        visit_openclaw_reader_lines(
            BufReader::new(file),
            session_id,
            file_mtime_ms,
            &mut current_model,
            &mut current_provider,
            &mut buffer,
            sink,
        )
    }
}

/// Streaming `.zst` decode: one lossy line plus decoder buffering in memory,
/// bounded by the stdlib `Take` to `MAX_ARCHIVE_BYTES + 1` decoded bytes.
/// `Take` caps what the `BufReader` can pull no matter how far ahead it
/// reads, so its overread cannot reject an archive at or under the bound;
/// after the loop `limit() == 0` means the decoder still had output, i.e.
/// the archive is oversize. Oversize, corrupt, or truncated archives return
/// `Err` so the transactional worker rolls back anything already sunk.
fn visit_openclaw_archive_lines(
    session_path: &Path,
    session_id: &str,
    file_mtime_ms: i64,
    current_model: &mut Option<String>,
    current_provider: &mut Option<String>,
    buffer: &mut Vec<u8>,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let file = super::utils::open_source_file(session_path).map_err(|error| error.to_string())?;
    let mut decoder = zstd::stream::read::Decoder::new(file).map_err(|error| error.to_string())?;
    decoder
        .window_log_max(26)
        .map_err(|error| error.to_string())?;
    let mut limited = decoder.take(MAX_ARCHIVE_BYTES + 1);
    {
        let mut reader = BufReader::new(&mut limited);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => return Err(error.to_string()),
            }
            strip_newline(&mut raw);
            let line = String::from_utf8_lossy(&raw).into_owned();
            visit_openclaw_line(
                &line,
                session_id,
                file_mtime_ms,
                current_model,
                current_provider,
                buffer,
                sink,
            )?;
        }
    }
    if limited.limit() == 0 {
        return Err("decoded transcript exceeds archive limit".to_string());
    }
    Ok(())
}

/// Plain-file streaming line driver over any buffered reader.
fn visit_openclaw_reader_lines<R: BufRead>(
    reader: R,
    session_id: &str,
    file_mtime_ms: i64,
    current_model: &mut Option<String>,
    current_provider: &mut Option<String>,
    buffer: &mut Vec<u8>,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let mut reader = reader;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(error.to_string()),
        }
        strip_newline(&mut raw);
        // Plain transcripts keep the legacy strict-UTF-8 skip: an undecodable
        // line costs only itself.
        let line = match String::from_utf8(std::mem::take(&mut raw)) {
            Ok(line) => line,
            Err(_) => continue,
        };
        visit_openclaw_line(
            &line,
            session_id,
            file_mtime_ms,
            current_model,
            current_provider,
            buffer,
            sink,
        )?;
    }
}

/// Drop one trailing `\n` plus an optional preceding `\r`, mirroring the
/// legacy plain-file framing (CRLF included).
fn strip_newline(raw: &mut Vec<u8>) {
    if raw.last() == Some(&b'\n') {
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
    }
}

/// One transcript line: model tracking plus a single assistant message to the
/// sink. Malformed lines are skipped; only the sink can fail.
#[allow(clippy::too_many_arguments)]
fn visit_openclaw_line(
    line: &str,
    session_id: &str,
    file_mtime_ms: i64,
    current_model: &mut Option<String>,
    current_provider: &mut Option<String>,
    buffer: &mut Vec<u8>,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    buffer.clear();
    buffer.extend_from_slice(trimmed.as_bytes());
    let entry: OpenClawEntry = match simd_json::from_slice(buffer) {
        Ok(entry) => entry,
        Err(_) => return Ok(()),
    };
    visit_openclaw_entry(
        entry,
        session_id,
        file_mtime_ms,
        current_model,
        current_provider,
        sink,
    )
}

/// One parsed entry: model tracking plus a single assistant message.
fn visit_openclaw_entry(
    entry: OpenClawEntry,
    session_id: &str,
    file_mtime_ms: i64,
    current_model: &mut Option<String>,
    current_provider: &mut Option<String>,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    match entry.entry_type.as_str() {
        "model_change" => {
            if let Some(model) = entry.model_id {
                *current_model = Some(model);
            }
            if let Some(provider) = entry.provider {
                *current_provider = Some(provider);
            }
            Ok(())
        }
        "custom" => {
            if entry.custom_type.as_deref() != Some("model-snapshot") {
                return Ok(());
            }
            if let Some(data) = entry.data {
                if let Some(model) = data.model_id {
                    *current_model = Some(model);
                }
                if let Some(provider) = data.provider {
                    *current_provider = Some(provider);
                }
            }
            Ok(())
        }
        "message" => visit_openclaw_message(
            entry.message,
            session_id,
            file_mtime_ms,
            current_model,
            current_provider,
            sink,
        ),
        _ => Ok(()),
    }
}

/// One `message` entry: assistant usage with a known model goes to the sink.
/// The timestamp is message-level (`message.timestamp`), falling back to the
/// file mtime, exactly like the legacy collector.
fn visit_openclaw_message(
    message: Option<OpenClawMessage>,
    session_id: &str,
    file_mtime_ms: i64,
    current_model: &mut Option<String>,
    current_provider: &mut Option<String>,
    sink: &mut dyn FnMut(UnifiedMessage) -> Result<(), String>,
) -> Result<(), String> {
    let message = match message {
        Some(message) => message,
        None => return Ok(()),
    };
    if message.role.as_deref() != Some("assistant") {
        return Ok(());
    }
    // Resolve model/provider against the borrowed message before moving its
    // usage out; a partial move would forbid the borrow below.
    let (model, provider) = openclaw_model_provider(&message, current_model, current_provider);
    let model = match model {
        Some(model) => model,
        None => return Ok(()),
    };
    let timestamp = message.timestamp.unwrap_or(file_mtime_ms);
    let usage = match message.usage {
        Some(usage) => usage,
        None => return Ok(()),
    };
    *current_model = Some(model.clone());
    *current_provider = Some(provider.clone());
    sink(openclaw_message(
        session_id, timestamp, model, provider, usage,
    ))
}

/// Resolve the message model/provider against the running lane state.
fn openclaw_model_provider(
    message: &OpenClawMessage,
    current_model: &Option<String>,
    current_provider: &Option<String>,
) -> (Option<String>, String) {
    let model = message
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .or_else(|| current_model.clone().filter(|model| !model.is_empty()));
    let provider = message
        .provider
        .clone()
        .filter(|provider| !provider.is_empty())
        .or_else(|| {
            current_provider
                .clone()
                .filter(|provider| !provider.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    (model, provider)
}
/// Build one provider-cost-aware assistant message.
fn openclaw_message(
    session_id: &str,
    timestamp: i64,
    model: String,
    provider: String,
    usage: OpenClawUsage,
) -> UnifiedMessage {
    let provider_cost = usage
        .cost
        .and_then(|cost| cost.total)
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    let mut message = UnifiedMessage::new(
        "openclaw",
        model,
        provider,
        session_id.to_string(),
        timestamp,
        TokenBreakdown {
            input: usage.input.unwrap_or(0).max(0),
            output: usage.output.unwrap_or(0).max(0),
            cache_read: usage.cache_read.unwrap_or(0).max(0),
            cache_write: usage.cache_write.unwrap_or(0).max(0),
            reasoning: 0,
            cache_write_1h: 0,
        },
        provider_cost.unwrap_or(0.0),
    );
    if provider_cost.is_some() {
        message.mark_provider_reported_cost();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_session(dir: &TempDir, filename: &str, content: &str) -> String {
        let path = dir.path().join(filename);
        let mut file = File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        path.to_string_lossy().to_string()
    }

    // Ported from upstream #1285.
    #[test]
    fn compressed_archives_preserve_transcript_usage_and_session_identity() {
        let dir = TempDir::new().unwrap();
        let content = concat!(
            "{\"type\":\"model_change\",\"provider\":\"anthropic\",\"modelId\":\"claude-sonnet-4-6\"}\r\n",
            "\ninvalid json\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":100,\"output\":50,\"cacheRead\":200,\"cacheWrite\":10,\"cost\":{\"total\":0.05}},\"timestamp\":1788566869012}}\n",
        );
        let plain = create_test_session(&dir, "session.jsonl", content);
        let expected = parse_openclaw_transcript(Path::new(&plain));
        assert_eq!(expected.len(), 1);

        for filename in [
            "session.jsonl.zst",
            "session.jsonl.deleted.2026-09-05T00-00-00.000Z.nonce.zst",
            "session.jsonl.reset.2026-09-05T00-00-00.000Z.nonce.zst",
        ] {
            let path = dir.path().join(filename);
            std::fs::write(&path, zstd::encode_all(content.as_bytes(), 0).unwrap()).unwrap();
            assert_eq!(parse_openclaw_transcript(&path), expected, "{filename}");
        }
    }

    // Ported from upstream #1285.
    // The 64 MiB decoded bound is exact: MAX decodes, MAX + 1 fails. Both
    // exercise the real streaming visitor, not a byte helper.
    #[test]
    fn archive_decoded_bound_accepts_exact_max_and_rejects_max_plus_one() {
        let dir = TempDir::new().unwrap();
        let line = b"{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"model\":\"m\",\"usage\":{\"input\":1},\"timestamp\":1700000000000}}\n";
        let filler = b"x\n";
        let mut exact = Vec::with_capacity(MAX_ARCHIVE_BYTES as usize + 1);
        exact.extend_from_slice(line);
        while exact.len() + filler.len() <= MAX_ARCHIVE_BYTES as usize {
            exact.extend_from_slice(filler);
        }
        while exact.len() < MAX_ARCHIVE_BYTES as usize {
            exact.push(b'x');
        }
        assert_eq!(exact.len(), MAX_ARCHIVE_BYTES as usize);
        let mut oversize = exact.clone();
        oversize.push(b'z');
        assert_eq!(oversize.len(), MAX_ARCHIVE_BYTES as usize + 1);

        let exact_path = dir.path().join("exact.jsonl.zst");
        std::fs::write(&exact_path, zstd::encode_all(exact.as_slice(), 0).unwrap()).unwrap();
        let mut count = 0;
        let result = visit_openclaw_session(&exact_path, "exact", &mut |_| {
            count += 1;
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(count, 1);
        assert_eq!(parse_openclaw_transcript(&exact_path).len(), 1);

        let oversize_path = dir.path().join("oversize.jsonl.zst");
        std::fs::write(
            &oversize_path,
            zstd::encode_all(oversize.as_slice(), 0).unwrap(),
        )
        .unwrap();
        let mut count = 0;
        let result = visit_openclaw_session(&oversize_path, "oversize", &mut |_| {
            count += 1;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(count, 1);
        assert!(parse_openclaw_transcript(&oversize_path).is_empty());
    }
    // Ported from upstream #1285.
    #[test]
    fn invalid_or_truncated_compressed_archive_does_not_emit_partial_usage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl.deleted.timestamp.zst");
        let line = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"model\":\"example\",\"usage\":{\"input\":100},\"timestamp\":1700000000000}}\n";
        let content = format!("{line}{line}");
        let mut bytes = zstd::encode_all(content.as_bytes(), 0).unwrap();
        bytes.pop();
        std::fs::write(&path, &bytes).unwrap();
        // A late corruption still fails the visit so the transactional worker
        // rolls back the valid prefix; the compatible API reports nothing.
        let mut count = 0;
        let result = visit_openclaw_session(&path, "session", &mut |_| {
            count += 1;
            Ok(())
        });
        assert!(result.is_err());
        assert!(parse_openclaw_transcript(&path).is_empty());
        std::fs::write(&path, b"not a zstd stream").unwrap();
        let result = visit_openclaw_session(&path, "session", &mut |_| Ok(()));
        assert!(result.is_err());
        assert!(parse_openclaw_transcript(&path).is_empty());
    }

    #[test]
    fn plain_transcript_keeps_reading_past_an_invalid_utf8_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"model\":\"m\",\"usage\":{\"input\":1},\"timestamp\":1700000000000}}\n");
        bytes.extend_from_slice(b"\xff\xfe not utf-8\n");
        bytes.extend_from_slice(b"{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"model\":\"m\",\"usage\":{\"input\":2},\"timestamp\":1700000001000}}\n");
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(parse_openclaw_transcript(&path).len(), 2);
    }
    #[test]
    fn transcript_without_filename_identity_is_empty_success() {
        let dir = TempDir::new().unwrap();
        let session_path = create_test_session(&dir, "no-identity.txt", "{\"type\":\"message\"}\n");
        assert!(parse_openclaw_transcript(Path::new(&session_path)).is_empty());
        let mut called = false;
        let result = visit_openclaw_transcript(Path::new(&session_path), &mut |_| {
            called = true;
            Ok(())
        });
        assert!(result.is_ok());
        assert!(!called);
    }

    #[test]
    fn sink_failure_aborts_session_visit() {
        let dir = TempDir::new().unwrap();
        let content = "{\"type\":\"model_change\",\"provider\":\"anthropic\",\"modelId\":\"m\"}\n{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1},\"timestamp\":1700000000000}}\n{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":2},\"timestamp\":1700000001000}}\n";
        let session_path = create_test_session(&dir, "session.jsonl", content);
        let mut calls = 0;
        let result = visit_openclaw_session(Path::new(&session_path), "test-session", &mut |_| {
            calls += 1;
            Err("boom".to_string())
        });
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
    #[test]
    fn message_timestamp_is_preserved_not_file_mtime() {
        let dir = TempDir::new().unwrap();
        let content = "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"model\":\"m\",\"usage\":{\"input\":7},\"timestamp\":1700000001234}}\n";
        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_transcript(Path::new(&session_path));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1700000001234);
    }

    #[test]
    fn test_parse_openclaw_session_with_model_change() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","id":"abc","provider":"openai-codex","modelId":"gpt-5.2"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50,"cacheRead":200,"totalTokens":350,"cost":{"total":0.05}},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-5.2");
        assert_eq!(messages[0].provider_id, "openai-codex");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(messages[0].tokens.cache_read, 200);
        assert_eq!(messages[0].cost, 0.05);
        assert!(messages[0].has_authoritative_cost());
    }

    #[test]
    fn test_parse_openclaw_session_user_messages_ignored() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3.5-sonnet"}
{"type":"message","id":"msg1","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}
{"type":"message","id":"msg2","message":{"role":"assistant","content":[],"usage":{"input":50,"output":25},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 50);
        assert!(!messages[0].has_authoritative_cost());
    }

    #[test]
    fn test_parse_openclaw_session_zero_cost_is_provider_reported() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3.5-sonnet"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":50,"output":25,"cost":{"total":0.0}},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].cost, 0.0);
        assert!(messages[0].has_authoritative_cost());
    }

    #[test]
    fn test_parse_openclaw_session_no_model_change() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 0);
    }

    #[test]
    fn test_parse_openclaw_transcript_derives_session_id_from_filename() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"openai-codex","modelId":"gpt-5.2"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "my-session-123.jsonl", content);
        let messages = parse_openclaw_transcript(Path::new(&session_path));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "my-session-123");
        assert_eq!(messages[0].model_id, "gpt-5.2");
        assert_eq!(messages[0].provider_id, "openai-codex");
        assert_eq!(messages[0].tokens.input, 10);
        assert_eq!(messages[0].tokens.output, 5);
    }

    #[test]
    fn test_parse_openclaw_transcript_derives_session_id_from_archived_filename() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"openai-codex","modelId":"gpt-5.2"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#;

        let session_path =
            create_test_session(&dir, "my-session-123.jsonl.deleted.1700000000000", content);
        let messages = parse_openclaw_transcript(Path::new(&session_path));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "my-session-123");
        assert_eq!(messages[0].model_id, "gpt-5.2");
        assert_eq!(messages[0].provider_id, "openai-codex");
        assert_eq!(messages[0].tokens.input, 10);
        assert_eq!(messages[0].tokens.output, 5);
    }

    #[test]
    fn test_parse_openclaw_transcript_derives_session_id_from_reset_filename() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-opus-4-6"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":10,"output":5,"cacheRead":1,"cacheWrite":2},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(
            &dir,
            "my-session-123.jsonl.reset.2026-03-20T06-34-44.520Z",
            content,
        );
        let messages = parse_openclaw_transcript(Path::new(&session_path));

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "my-session-123");
        assert_eq!(messages[0].model_id, "claude-opus-4-6");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    #[test]
    fn test_parse_openclaw_session_model_snapshot_updates_current_model() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"custom","customType":"model-snapshot","data":{"provider":"anthropic","modelId":"claude-opus-4-6"}}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50,"cacheRead":25,"cacheWrite":10},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-opus-4-6");
        assert_eq!(messages[0].provider_id, "anthropic");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(messages[0].tokens.cache_read, 25);
        assert_eq!(messages[0].tokens.cache_write, 10);
    }

    #[test]
    fn test_parse_openclaw_session_embedded_model_provider_without_model_change() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"message","id":"msg1","message":{"role":"assistant","provider":"anthropic","model":"claude-sonnet-4-6","content":[],"usage":{"input":100,"output":50,"cacheRead":20,"cacheWrite":5},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-6");
        assert_eq!(messages[0].provider_id, "anthropic");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.cache_write, 5);
    }

    #[test]
    fn test_parse_openclaw_session_preserves_unknown_provider_fallback() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","modelId":"claude-sonnet-4-6"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":10,"output":5},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-sonnet-4-6");
        assert_eq!(messages[0].provider_id, "unknown");
    }

    #[test]
    fn test_parse_openclaw_session_empty_embedded_values_fall_back_to_current_model_state() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-opus-4-6"}
{"type":"message","id":"msg1","message":{"role":"assistant","provider":"","model":"","content":[],"usage":{"input":10,"output":5},"timestamp":1700000000000}}"#;

        let session_path = create_test_session(&dir, "session.jsonl", content);
        let messages = parse_openclaw_session(Path::new(&session_path), "test-session");

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-opus-4-6");
        assert_eq!(messages[0].provider_id, "anthropic");
    }

    fn create_test_index(dir: &TempDir, content: &str) -> PathBuf {
        let index_path = dir.path().join("sessions.json");
        let mut file = File::create(&index_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        index_path
    }

    #[test]
    fn test_parse_openclaw_index_absolute_session_file() {
        let dir = TempDir::new().unwrap();

        let session_content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3.5-sonnet"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#;
        let session_path = create_test_session(&dir, "session-abc.jsonl", session_content);

        let index_content = format!(
            r#"{{
            "agent:main:main": {{
                "sessionId": "abc-123",
                "sessionFile": "{}"
            }}
        }}"#,
            session_path.replace('\\', "\\\\")
        );
        let index_path = create_test_index(&dir, &index_content);

        let messages = parse_openclaw_index(&index_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-3.5-sonnet");
        assert_eq!(messages[0].session_id, "abc-123");
    }

    #[test]
    fn test_parse_openclaw_index_relative_session_file() {
        let dir = TempDir::new().unwrap();

        let session_content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3.5-sonnet"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#;
        create_test_session(&dir, "session-relative.jsonl", session_content);

        let index_content = r#"{
            "agent:main:main": {
                "sessionId": "relative-123",
                "sessionFile": "session-relative.jsonl"
            }
        }"#;
        let index_path = create_test_index(&dir, index_content);

        let messages = parse_openclaw_index(&index_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-3.5-sonnet");
        assert_eq!(messages[0].session_id, "relative-123");
    }

    #[test]
    fn test_parse_openclaw_index_missing_session_file_fallback() {
        let dir = TempDir::new().unwrap();

        let session_content = r#"{"type":"model_change","provider":"anthropic","modelId":"claude-3.5-sonnet"}
{"type":"message","id":"msg1","message":{"role":"assistant","content":[],"usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0},"timestamp":1700000000000}}"#;
        create_test_session(&dir, "fallback-123.jsonl", session_content);

        let index_content = r#"{
            "agent:main:main": {
                "sessionId": "fallback-123"
            }
        }"#;
        let index_path = create_test_index(&dir, index_content);

        let messages = parse_openclaw_index(&index_path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "claude-3.5-sonnet");
        assert_eq!(messages[0].session_id, "fallback-123");
    }
}
