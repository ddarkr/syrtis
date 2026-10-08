use serde_json::json;
use std::path::Path;
use syrtis_core::{parse_local_clients, LocalParseOptions, ReportOptions};

fn write_usage(path: &Path, split: bool) {
    let usage = if split {
        json!({"input_tokens":265,"cache_read_input_tokens":42000,"output_tokens":120})
    } else {
        json!({"input_tokens":42494,"output_tokens":120})
    };
    std::fs::write(path, json!({"type":"assistant","timestamp":"2026-01-01T10:00:00Z",
        "requestId":"same-request","message":{"id":"same-message","model":"claude-sonnet-4-6","usage":usage}}).to_string()+"\n").unwrap();
}

#[test]
fn rewritten_claude_units_agree_across_changed_warm_and_compacted_reports() {
    if std::env::var_os("TOKSCALE_REWRITE_PROOF_CHILD").is_none() {
        for streaming_first in [false, true] {
            for initially_split in [false, true] {
                let home = tempfile::tempdir().unwrap();
                // Windows resolves SQLite scratch/tempfile paths from TEMP/TMP;
                // env_clear would otherwise leave machine-global values in play.
                let tmp = home.path().join("tmp");
                std::fs::create_dir_all(&tmp).unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "rewritten_claude_units_agree_across_changed_warm_and_compacted_reports",
                        "--nocapture",
                    ])
                    .env_clear()
                    .env("TOKSCALE_REWRITE_PROOF_CHILD", "1")
                    .env(
                        "TOKSCALE_PROOF_STREAMING_FIRST",
                        if streaming_first { "1" } else { "0" },
                    )
                    .env(
                        "TOKSCALE_PROOF_INITIAL_SPLIT",
                        if initially_split { "1" } else { "0" },
                    )
                    .env("HOME", home.path())
                    .env("TEMP", &tmp)
                    .env("TMP", &tmp)
                    .env("TMPDIR", &tmp)
                    .env("TOKSCALE_CONFIG_DIR", home.path().join("config"));
                for key in ["SystemRoot", "SystemDrive"] {
                    if let Some(value) = std::env::var_os(key) {
                        command.env(key, value);
                    }
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "initially_split={initially_split}, streaming_first={streaming_first}\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let source = home.join(".claude/projects/proof/session.jsonl");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let options = || LocalParseOptions {
        home_dir: Some(home.to_string_lossy().into_owned()),
        use_env_roots: false,
        clients: Some(vec!["claude".into()]),
        ..Default::default()
    };
    let initially_split = std::env::var("TOKSCALE_PROOF_INITIAL_SPLIT").unwrap() == "1";
    write_usage(&source, initially_split);
    parse_local_clients(options()).unwrap();
    write_usage(&source, !initially_split);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let graph = || {
        runtime
            .block_on(syrtis_core::generate_local_graph_report_local_only(
                ReportOptions {
                    home_dir: options().home_dir,
                    clients: options().clients,
                    use_env_roots: false,
                    ..Default::default()
                },
            ))
            .unwrap()
            .into_graph()
    };
    // The streaming-first lane must agree on the changed fingerprint, before
    // any materialized pass has populated the cache for this generation.
    if std::env::var("TOKSCALE_PROOF_STREAMING_FIRST").unwrap() == "1" {
        let streamed: Vec<syrtis_core::UnifiedMessage> = runtime
            .block_on(syrtis_core::parse_local_unified_messages(options()))
            .unwrap();
        assert_eq!(streamed.len(), 1);
        assert_eq!(
            (
                streamed[0].tokens.input,
                streamed[0].tokens.cache_read,
                streamed[0].tokens.output
            ),
            (265, 42000, 120),
        );
        assert_eq!(streamed[0].tokens.total(), 42385);
        assert_eq!(graph().summary.total_tokens, 42385);
    }
    for compacted in [false, true] {
        if compacted {
            std::fs::write(
                &source,
                "{\"type\":\"summary\",\"summary\":\"compacted\"}\n",
            )
            .unwrap();
        }
        for _ in 0..2 {
            let parsed = parse_local_clients(options()).unwrap();
            assert_eq!(parsed.messages.len(), 1);
            assert_eq!(
                (
                    parsed.messages[0].input,
                    parsed.messages[0].cache_read,
                    parsed.messages[0].output
                ),
                (265, 42000, 120)
            );
            assert_eq!(graph().summary.total_tokens, 42385);
            // The streaming lane must agree with the materialized lanes above
            // against the same persisted generation, in both lane orders.
            let streamed: Vec<syrtis_core::UnifiedMessage> = runtime
                .block_on(syrtis_core::parse_local_unified_messages(options()))
                .unwrap();
            assert_eq!(streamed.len(), 1);
            assert_eq!(streamed[0].tokens.total(), 42385);
            assert_eq!(
                (
                    streamed[0].tokens.input,
                    streamed[0].tokens.cache_read,
                    streamed[0].tokens.output
                ),
                (265, 42000, 120),
            );
        }
    }
}
