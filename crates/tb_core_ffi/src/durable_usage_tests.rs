//! Consumer-visible persistence: new native leases must preserve collected usage
//! after deletion, truncation, replay and process restart.

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use std::ffi::{CStr, CString};
    use std::path::Path;

    fn data(raw: *mut std::ffi::c_char) -> Value {
        assert!(!raw.is_null());
        let value: Value =
            serde_json::from_slice(unsafe { CStr::from_ptr(raw) }.to_bytes()).unwrap();
        unsafe { crate::tb_free(raw) };
        assert_eq!(value["ok"], true, "{value}");
        value["data"].clone()
    }

    fn row(key: &str, input: i64) -> String {
        json!({"type":"assistant","timestamp":"2026-10-08T00:00:00Z","requestId":key,
            "message":{"id":key,"model":"claude-sonnet-4","usage":{
                "input_tokens":input,"output_tokens":20,"cache_read_input_tokens":5,
                "cache_creation_input_tokens":3}}})
        .to_string()
            + "\n"
    }

    fn assert_reports(expected_tokens: i64, expected_messages: i64) {
        let request = CString::new(
            json!({"force":true,"scopes":[
                {"id":"all","kind":"report"},
                {"id":"claude","kind":"report","clients":["claude"]}
            ]})
            .to_string(),
        )
        .unwrap();
        let synced = data(unsafe { crate::tb_local_usage_sync(request.as_ptr()) });
        let token = synced["leaseToken"].as_str().unwrap();
        let query = |scope: &str, kind: &str| {
            let request =
                CString::new(json!({"leaseToken":token,"scopeId":scope,"kind":kind}).to_string())
                    .unwrap();
            data(unsafe { crate::tb_local_usage_query(request.as_ptr()) })
        };
        assert_eq!(
            query("all", "graph")["summary"]["totalTokens"],
            expected_tokens
        );
        assert_eq!(query("all", "models")["totalMessages"], expected_messages);
        let hourly = query("claude", "hourly");
        let tokens: i64 = hourly["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["total"].as_i64().unwrap())
            .sum();
        assert_eq!(tokens, expected_tokens);
        assert_eq!(
            query("claude", "agents")["totalMessages"],
            expected_messages
        );
        let token = CString::new(token).unwrap();
        assert_eq!(
            data(unsafe { crate::tb_local_usage_release(token.as_ptr()) })["released"],
            true
        );
    }

    fn run_child(root: &Path, tokens: i64, messages: i64) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "durable_usage_tests::tests::collected_usage_survives_source_loss_and_restart",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", root)
            .env("SYRTIS_CONFIG_DIR", root.join("config"))
            .env("TOKSCALE_CONFIG_DIR", root.join("config"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .env("XDG_CONFIG_HOME", root.join("xdg-config"))
            .env("XDG_DATA_HOME", root.join("xdg-data"))
            .env("SYRTIS_DURABLE_CHILD_TOKENS", tokens.to_string())
            .env("SYRTIS_DURABLE_CHILD_MESSAGES", messages.to_string())
            .env("TZ", "UTC")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn collected_usage_survives_source_loss_and_restart() {
        if let Ok(tokens) = std::env::var("SYRTIS_DURABLE_CHILD_TOKENS") {
            assert_reports(
                tokens.parse().unwrap(),
                std::env::var("SYRTIS_DURABLE_CHILD_MESSAGES")
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
            return;
        }
        let root = tempfile::TempDir::new().unwrap();
        let directory = root.path().join(".claude/projects/proof");
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("session.jsonl");
        let initial = row("persist-a", 10) + &row("persist-b", 50);
        std::fs::write(&source, &initial).unwrap();
        run_child(root.path(), 116, 2);

        // Compaction removes one collected turn, but must not erase its usage.
        std::fs::write(&source, row("persist-b", 50)).unwrap();
        run_child(root.path(), 116, 2);
        let all = initial + &row("persist-c", 100);
        std::fs::write(&source, &all).unwrap();
        run_child(root.path(), 244, 3);

        std::fs::remove_dir_all(root.path().join(".claude")).unwrap();
        run_child(root.path(), 244, 3);
        // A second fresh process must not depend on the previous process's lease.
        run_child(root.path(), 244, 3);

        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&source, &all).unwrap();
        std::fs::write(directory.join("copied.jsonl"), &all).unwrap();
        run_child(root.path(), 244, 3);
    }
}
