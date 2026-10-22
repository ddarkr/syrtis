#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::ffi::{CStr, CString};

    thread_local! {
        static AFTER_SOURCE_SYNC: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            std::cell::RefCell::new(None);
    }

    pub(super) fn after_source_sync() {
        let hook = AFTER_SOURCE_SYNC.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    fn ffi(
        request: &Value,
        call: unsafe extern "C" fn(*const std::ffi::c_char) -> *mut std::ffi::c_char,
    ) -> Value {
        let request = CString::new(request.to_string()).unwrap();
        let raw = unsafe { call(request.as_ptr()) };
        assert!(!raw.is_null());
        let result = serde_json::from_slice(unsafe { CStr::from_ptr(raw) }.to_bytes());
        unsafe { crate::tb_free(raw) };
        result.unwrap()
    }
    fn data(result: Value) -> Value {
        assert_eq!(result["ok"], true, "{result}");
        result["data"].clone()
    }
    fn query_at(token: &str, scope: &str, kind: &str, now: i64) -> Value {
        data(ffi(
            &json!({"leaseToken":token,"scopeId":scope,"kind":kind,"nowMs":now}),
            crate::tb_local_usage_query,
        ))
    }
    fn release(token: &str) -> Value {
        let token = CString::new(token).unwrap();
        let raw = unsafe { crate::tb_local_usage_release(token.as_ptr()) };
        let result = serde_json::from_slice(unsafe { CStr::from_ptr(raw) }.to_bytes());
        unsafe { crate::tb_free(raw) };
        data(result.unwrap())
    }
    fn write_usage(path: &std::path::Path, timestamp: i64, input: i64) {
        let date = chrono::DateTime::from_timestamp_millis(timestamp)
            .unwrap()
            .to_rfc3339();
        std::fs::write(path, json!({"type":"assistant","timestamp":date,"requestId":"scheduler-proof", "message":{
            "id":"scheduler-proof","model":"claude-sonnet-4","usage":{
                "input_tokens":input,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3
            }}}).to_string()+"\n").unwrap();
    }

    #[test]
    fn recent_trace_rate_matches_independent_folds_and_lease_admission() {
        if std::env::var_os("SYRTIS_TRACE_RATE_CHILD").is_none() {
            let root = tempfile::TempDir::new().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "local_usage::tests::recent_trace_rate_matches_independent_folds_and_lease_admission", "--nocapture"])
                .env_clear()
                .env("SYRTIS_TRACE_RATE_CHILD", "1")
                .env("HOME", root.path())
                .env("TOKSCALE_CONFIG_DIR", root.path().join("config"))
                .env("XDG_CONFIG_HOME", root.path().join("xdg-config"))
                .env("XDG_DATA_HOME", root.path().join("xdg-data"))
                .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap())
            .join("opencode/storage/message/session");
        std::fs::create_dir_all(&directory).unwrap();
        let now = now_ms();
        for (id, timestamp, model, input) in [
            ("ordinary", now - 120_000, "ordinary", 41),
            ("lower", now - 60_001, "boundary", 97),
            ("first", now - 60_000, "shared", i64::MAX),
            ("second", now - 30_000, "other", 17),
            ("third", now, "shared", i64::MIN),
            ("future", now + 1, "future", 101),
        ] {
            std::fs::write(
                directory.join(format!("{id}.json")),
                json!({
                    "id":id, "sessionID":"session", "role":"assistant", "modelID":model,
                    "providerID":"openai", "mode":"build", "time":{"created":timestamp},
                    "tokens":{"input":input,"output":0,"reasoning":0,"cache":{"read":0,"write":0}}
                })
                .to_string(),
            )
            .unwrap();
        }
        let synced = data(ffi(
            &json!({"scopes":[
                {"id":"live","kind":"recent","clients":["opencode"]},
                {"id":"report","kind":"report","clients":["opencode"]}
            ]}),
            crate::tb_local_usage_sync,
        ));
        let token = synced["leaseToken"].as_str().unwrap();
        let query = |scope: &str, kind: &str, window: i64, clock: i64| {
            ffi(
                &json!({"leaseToken":token,"scopeId":scope,"kind":kind,
                "windowSecs":window,"nowMs":clock}),
                crate::tb_local_usage_query,
            )
        };
        let normalized_trace = |value: &Value| {
            let mut rows = value.as_array().unwrap().clone();
            rows.sort_by_key(|row| {
                (
                    row["client"].as_str().unwrap().to_owned(),
                    row["agent"].as_str().unwrap().to_owned(),
                    row["model"].as_str().unwrap().to_owned(),
                )
            });
            rows
        };
        for (window, clock) in [
            (60, now),
            (60, now - 1),
            (60, now + 1),
            (1, now - 120_000),
            (600, now),
            (0, now),
            (-1, now),
            (i64::MAX, i64::MAX),
            (60, i64::MIN),
        ] {
            let combined_response = query("live", "traceRate", window, clock);
            let rate_response = query("live", "rate", window, clock);
            let trace_response = query("live", "trace", window, clock);
            assert_eq!(
                combined_response["ok"], rate_response["ok"],
                "{window}/{clock}"
            );
            assert_eq!(
                combined_response["ok"], trace_response["ok"],
                "{window}/{clock}"
            );
            if combined_response["ok"] == false {
                assert_eq!(combined_response, rate_response, "{window}/{clock}");
                assert_eq!(combined_response, trace_response, "{window}/{clock}");
                continue;
            }
            let combined = data(combined_response);
            let rate = data(rate_response);
            let trace = data(trace_response);
            assert_eq!(
                (combined["rate"].as_f64().unwrap() as f32).to_bits(),
                (rate.as_f64().unwrap() as f32).to_bits(),
                "{window}/{clock}"
            );
            assert_eq!(
                normalized_trace(&combined["trace"]),
                normalized_trace(&trace),
                "{window}/{clock}"
            );
            let rows = combined["trace"].as_array().unwrap();
            assert!(rows
                .windows(2)
                .all(|pair| pair[0]["tokens"].as_i64().unwrap()
                    >= pair[1]["tokens"].as_i64().unwrap()));
        }
        // OpenCode clamps negative source token lanes before publication.
        let combined = data(query("live", "traceRate", 60, now));
        assert_eq!(
            (combined["rate"].as_f64().unwrap() as f32).to_bits(),
            (i64::MAX as f32).to_bits()
        );
        assert_eq!(
            combined["trace"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["model"] == "shared")
                .unwrap()["tokens"],
            i64::MAX
        );
        assert_eq!(query("report", "traceRate", 60, now)["ok"], false);
        assert_eq!(query("missing", "traceRate", 60, now)["ok"], false);
        let mismatch = ffi(
            &json!({"leaseToken":token,"scopeId":"live","kind":"traceRate","clients":["claude"]}),
            crate::tb_local_usage_query,
        );
        assert_eq!(mismatch["ok"], false);
        assert!(release(token)["released"].as_bool().unwrap());
        assert_eq!(query("live", "traceRate", 60, now)["ok"], false);
    }

    #[test]
    fn scheduler_pinned_queries_preserve_scopes_clocks_and_independent_owners() {
        if std::env::var_os("SYRTIS_LOCAL_SCHEDULER_CHILD").is_none() {
            let root = tempfile::TempDir::new().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "local_usage::tests::scheduler_pinned_queries_preserve_scopes_clocks_and_independent_owners", "--nocapture"])
                .env_clear().env("SYRTIS_LOCAL_SCHEDULER_CHILD","1")
                .env("HOME",root.path()).env("TOKSCALE_CONFIG_DIR",root.path().join("config"))
                .env("XDG_CONFIG_HOME",root.path().join("xdg-config"))
                .env("XDG_DATA_HOME",root.path().join("xdg-data"))
                .env("APPDATA",root.path().join("roaming"))
                .env("LOCALAPPDATA",root.path().join("local"))
                .env("TOKSCALE_PRICING_CACHE_ONLY","1").env("TZ","UTC");
            for key in ["SystemRoot", "SystemDrive"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os("HOME").unwrap());
        let source = root.join(".claude/projects/proof/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let now = now_ms();
        let timestamp = now.saturating_sub(30_000);
        write_usage(&source, timestamp, 10);
        let cache = root.join("config/cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(
            cache.join("pricing-litellm.json"),
            json!({"timestamp":now/1000,"data":{"claude-sonnet-4":{
                "input_cost_per_token":0.001,"output_cost_per_token":0.002,
                "cache_read_input_token_cost":0.0001,"cache_creation_input_token_cost":0.00125
            }}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            cache.join("pricing-openrouter.json"),
            json!({"timestamp":now/1000,"data":{}}).to_string(),
        )
        .unwrap();
        let scopes = json!([
            {"id":"all","kind":"report"},
            {"id":"claude","kind":"report","clients":["claude"]},
            {"id":"live","kind":"recent"},
            {"id":"account","kind":"quota","clientId":"claude"}
        ]);
        let first = data(ffi(&json!({"scopes":scopes}), crate::tb_local_usage_sync));
        let token = first["leaseToken"].as_str().unwrap();
        assert_eq!(
            query_at(token, "all", "graph", now)["summary"]["totalTokens"],
            38
        );
        assert_eq!(query_at(token, "all", "models", now)["totalMessages"], 1);
        assert_eq!(
            query_at(token, "claude", "hourly", now)["entries"][0]["total"],
            38
        );
        assert_eq!(query_at(token, "claude", "agents", now)["totalMessages"], 1);
        // Same ordinary lease, impossible year: downstream year filtering must
        // hide the fixture rows on every lane instead of leaking the cached
        // unfiltered fold. Fixture timestamp is `now - 30s`, so year 1900 is
        // empty for all four lanes.
        let empty_year = "1900";
        let empty_graph = data(ffi(
            &json!({"leaseToken":token,"scopeId":"all","kind":"graph","year":empty_year,"nowMs":now}),
            crate::tb_local_usage_query,
        ));
        assert_eq!(empty_graph["summary"]["totalTokens"], 0);
        let empty_models = data(ffi(
            &json!({"leaseToken":token,"scopeId":"all","kind":"models","year":empty_year,"nowMs":now}),
            crate::tb_local_usage_query,
        ));
        assert_eq!(empty_models["totalMessages"], 0);
        let empty_hourly = data(ffi(
            &json!({"leaseToken":token,"scopeId":"claude","kind":"hourly","year":empty_year,"nowMs":now}),
            crate::tb_local_usage_query,
        ));
        assert_eq!(empty_hourly["entries"], json!([]));
        let empty_agents = data(ffi(
            &json!({"leaseToken":token,"scopeId":"claude","kind":"agents","year":empty_year,"nowMs":now}),
            crate::tb_local_usage_query,
        ));
        assert_eq!(empty_agents["totalMessages"], 0);
        assert_eq!(empty_agents["entries"], json!([]));
        assert_eq!(query_at(token, "live", "rate", now), 38.0);
        assert_eq!(query_at(token, "live", "trace", now)[0]["tokens"], 38);
        // A captured future event must not count until the query clock reaches
        // it. Preserve the existing inclusive current-millisecond boundary.
        assert_eq!(
            query_at(token, "live", "rate", timestamp.saturating_sub(1)),
            0.0
        );
        assert_eq!(
            query_at(token, "live", "trace", timestamp.saturating_sub(1)),
            json!([])
        );
        assert_eq!(query_at(token, "live", "rate", timestamp), 38.0);
        assert_eq!(query_at(token, "live", "trace", timestamp)[0]["tokens"], 38);
        assert_eq!(
            query_at(token, "live", "rate", now.saturating_add(61_000)),
            0.0
        );
        assert_eq!(
            query_at(token, "live", "trace", now.saturating_add(601_000)),
            json!([])
        );
        let mismatch = ffi(
            &json!({"leaseToken":token,"scopeId":"all","kind":"hourly","clients":["claude"]}),
            crate::tb_local_usage_query,
        );
        assert_eq!(
            mismatch["ok"], false,
            "mixed all-client fold must not answer an exact client scope"
        );
        let warm = data(ffi(
            &json!({"scopes":scopes,"previousLeaseToken":token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        assert_eq!(warm["leaseToken"], token);
        assert_eq!(warm["changed"], false);

        // Holding the source lane must not block compact same-token quota reads.
        let quota_token = first["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|scope| scope["id"] == "account")
            .unwrap()["quotaSnapshotToken"]
            .as_str()
            .unwrap()
            .to_owned();
        let lane = crate::local_usage_guard();
        let (sent, received) = std::sync::mpsc::channel();
        let quota_thread = std::thread::spawn(move || {
            let result = ffi(
                &json!({"accountKey":null,"fromMs":timestamp,"untilMs":now,
                "snapshotToken":quota_token}),
                crate::tb_window_usage,
            );
            sent.send(result).unwrap();
        });
        let pure = received.recv_timeout(std::time::Duration::from_secs(10));
        drop(lane);
        quota_thread.join().unwrap();
        assert_eq!(
            pure.unwrap()["ok"],
            true,
            "pure quota query entered source synchronization lane"
        );

        write_usage(&source, timestamp, 50);
        let refreshed = data(ffi(
            &json!({"scopes":scopes,"previousLeaseToken":token,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let refreshed_token = refreshed["leaseToken"].as_str().unwrap();
        assert_ne!(refreshed_token, token);
        assert_eq!(
            query_at(refreshed_token, "all", "graph", now)["summary"]["totalTokens"],
            78
        );
        let independent = data(ffi(&json!({"scopes":scopes}), crate::tb_local_usage_sync));
        let independent_token = independent["leaseToken"].as_str().unwrap();
        assert_ne!(independent_token, refreshed_token);
        let late_query = find_lease(refreshed_token).unwrap();
        assert_eq!(release(refreshed_token)["released"], true);
        assert!(admit(refreshed_token, &late_query, false).is_err());
        assert_eq!(
            ffi(
                &json!({"leaseToken":refreshed_token,"scopeId":"all","kind":"graph"}),
                crate::tb_local_usage_query
            )["ok"],
            false
        );
        assert_eq!(
            query_at(independent_token, "all", "graph", now)["summary"]["totalTokens"],
            78
        );
        assert_eq!(release(refreshed_token)["released"], false);
        release(token);
        release(independent_token);

        // Deterministically append AFTER real ingestion but BEFORE consumer
        // acceptance: no timing race, mock snapshot, or quiescence assumption.
        let exact_scope = json!([{"id":"claude","kind":"report","clients":["claude"]}]);
        write_usage(&source, timestamp, 90);
        let later_source = source.clone();
        AFTER_SOURCE_SYNC.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                write_usage(&later_source, timestamp, 130);
            }));
        });
        let captured = data(ffi(
            &json!({"scopes":exact_scope,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let captured_token = captured["leaseToken"].as_str().unwrap();
        assert_eq!(
            query_at(captured_token, "claude", "graph", now)["summary"]["totalTokens"],
            118,
            "a late external write must not invalidate the completed snapshot"
        );
        assert!(
            find_lease(captured_token).unwrap().scopes[0]
                .source
                .manifest
                .check_known_sources()
                .unwrap(),
            "acceptance must retain the pre-ingestion dirty baseline"
        );
        // The changed-source path must also keep a write that arrives after
        // its own ingestion dirty, without delaying this captured result.
        let later_source = source.clone();
        AFTER_SOURCE_SYNC.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                write_usage(&later_source, timestamp, 170);
            }));
        });
        let next = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":captured_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        let next_token = next["leaseToken"].as_str().unwrap();
        assert_ne!(next_token, captured_token);
        assert_eq!(next["changed"], true);
        assert_eq!(
            query_at(next_token, "claude", "graph", now)["summary"]["totalTokens"],
            158,
            "ordinary known-source verification must ingest the later write"
        );
        let later = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":next_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        assert_eq!(
            query_at(
                later["leaseToken"].as_str().unwrap(),
                "claude",
                "graph",
                now
            )["summary"]["totalTokens"],
            198,
            "a late write after targeted ingestion belongs to the next capture"
        );
        release(next_token);
        let next_token = later["leaseToken"].as_str().unwrap();
        let settled = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":next_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        assert_eq!(settled["leaseToken"], next_token);
        assert_eq!(settled["changed"], false);

        // Removing scope-wide quiescence must not turn actual failed reads
        // into successful empty coverage or damage the last-good snapshot.
        let database =
            PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap()).join("opencode/opencode.db");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        std::fs::write(&database, b"not a sqlite database").unwrap();
        let corrupt = ffi(
            &json!({"scopes":[{"id":"opencode","kind":"recent","clients":["opencode"]}]}),
            crate::tb_local_usage_sync,
        );
        assert_eq!(corrupt["ok"], false, "{corrupt}");
        assert!(!corrupt["err"].as_str().unwrap().is_empty());

        let sidechain = source.parent().unwrap().join("agent-side.jsonl");
        write_usage(&sidechain, timestamp, 1);
        let mut row: Value = serde_json::from_slice(&std::fs::read(&sidechain).unwrap()).unwrap();
        row["isSidechain"] = json!(true);
        row["sessionId"] = json!("parent");
        std::fs::write(&sidechain, row.to_string() + "\n").unwrap();
        std::fs::create_dir(sidechain.with_extension("meta.json")).unwrap();
        let unreadable = ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":next_token,"force":true}),
            crate::tb_local_usage_sync,
        );
        assert_eq!(unreadable["ok"], false, "{unreadable}");
        assert!(!unreadable["err"].as_str().unwrap().is_empty());
        assert_eq!(
            query_at(next_token, "claude", "graph", now)["summary"]["totalTokens"],
            198
        );
        release(captured_token);
        release(next_token);
        std::fs::remove_file(database).unwrap();
        std::fs::remove_dir(sidechain.with_extension("meta.json")).unwrap();
        std::fs::remove_file(sidechain).unwrap();

        // Primary quota without a client id is the real Swift wire shape
        // (kind quota, id quota:primary): accepted as the cross-client window.
        let omitted_primary = json!([{"id":"quota:primary","kind":"quota"}]);
        let primary_sync = data(ffi(
            &json!({"scopes":omitted_primary,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let primary_token = primary_sync["leaseToken"].as_str().unwrap().to_owned();
        assert!(
            primary_sync["scopes"][0]["quotaSnapshotToken"]
                .as_str()
                .is_some(),
            "omitted-clientId primary quota must register: {primary_sync}"
        );
        release(&primary_token);
        // Extras stay Claude-only: a non-Claude identity naming an account and
        // a bare account without a client are both invalid combinations.
        for bad in [
            json!([{"id":"q","kind":"quota","clientId":"codex","accountKey":"/tmp/extra"}]),
            json!([{"id":"q","kind":"quota","accountKey":"/tmp/extra"}]),
        ] {
            let rejected = ffi(&json!({"scopes":bad}), crate::tb_local_usage_sync);
            assert_eq!(rejected["ok"], false, "invalid extra accepted: {rejected}");
            assert!(!rejected["err"].as_str().unwrap().is_empty());
        }

        // Recent + primary quota with identical options share ONE capture
        // (exact resolved engine producer identity, not FFI kind): one
        // ingestion for two public scopes, and a late write between ingestion
        // and acceptance is visible to the next refresh from both scopes
        // without tainting this one.
        let shared = json!([
            {"id":"live","kind":"recent"},
            {"id":"quota:primary","kind":"quota"}
        ]);
        write_usage(&source, timestamp, 200);
        let later = source.clone();
        AFTER_SOURCE_SYNC.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                write_usage(&later, timestamp, 240);
            }));
        });
        let shared_sync = data(ffi(
            &json!({"scopes":shared,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let shared_token = shared_sync["leaseToken"].as_str().unwrap().to_owned();
        // Consumer admission: exactly two scope entries over one source
        // generation, both served from the single capture.
        assert_eq!(
            shared_sync["scopes"].as_array().unwrap().len(),
            2,
            "two public scopes admitted over one capture"
        );
        assert!(
            !query_at(&shared_token, "live", "trace", now)
                .as_array()
                .unwrap()
                .is_empty(),
            "shared producer must serve the recent trace"
        );
        assert!(
            shared_sync["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|scope| scope["id"] == "quota:primary")
                .unwrap()["quotaSnapshotToken"]
                .as_str()
                .is_some(),
            "shared capture must still register the quota card: {shared_sync}"
        );
        assert_eq!(
            shared_sync["scopes"][0]["sourceGeneration"],
            shared_sync["scopes"][1]["sourceGeneration"],
            "same producer must publish one source generation: {shared_sync}"
        );
        assert_eq!(
            query_at(&shared_token, "live", "rate", now),
            228.0,
            "shared recent+quota capture must serve the fresh rate"
        );
        let refresh = data(ffi(
            &json!({"scopes":shared,"previousLeaseToken":shared_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        let refresh_token = refresh["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(refresh_token, shared_token);
        assert_eq!(refresh["changed"], true);
        assert_ne!(
            refresh["scopes"][0]["sourceGeneration"], shared_sync["scopes"][0]["sourceGeneration"],
            "late write must advance the next generation for both scopes"
        );
        assert_eq!(
            query_at(&refresh_token, "live", "rate", now),
            268.0,
            "late write must reach the recent rate on the next refresh"
        );
        assert_eq!(
            query_at(&refresh_token, "live", "rate", now.saturating_add(61_000)),
            0.0,
            "recent rate must expire by the clock without a resync"
        );
        release(&shared_token);
        release(&refresh_token);

        // Differing exclusions isolate producers: registering an extra account
        // narrows the primary partition, so it must not reuse the unexcluded
        // generation even though both are recent+quota-shaped.
        let extra_dir = root.join("extra-claude");
        std::fs::create_dir_all(extra_dir.join("projects")).unwrap();
        crate::extra_scan_paths::set_from_json(
            &json!({"claude":[extra_dir.join("projects").to_str().unwrap()]}).to_string(),
        )
        .unwrap();
        crate::claude_config_dirs::set_from_json(&json!([extra_dir.to_str().unwrap()]).to_string())
            .unwrap();
        let narrow = data(ffi(
            &json!({"scopes":shared,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let narrow_token = narrow["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(
            narrow["scopes"][0]["sourceGeneration"], shared_sync["scopes"][0]["sourceGeneration"],
            "excluded primary must not reuse the unexcluded capture"
        );
        crate::extra_scan_paths::reset_for_test();
        crate::claude_config_dirs::reset_for_test();
        release(&narrow_token);

        // Capacity: 33 distinct producers fail BEFORE ingestion (no eviction
        // of earlier accepted state), while 64 public IDs sharing few
        // producers stay within the wire cap.
        let mut distinct = Vec::new();
        for index in 0..33 {
            distinct.push(json!({"id":format!("r{index}"),"kind":"recent","clients":[format!("unknown-client-{index}")]}));
        }
        // NOTE: engine captures resolve before ingestion, so this fails fast;
        // the previously accepted snapshots above remain queryable.
        let oversized = ffi(&json!({"scopes":distinct}), crate::tb_local_usage_sync);
        assert_eq!(
            oversized["ok"], false,
            "oversized capture accepted: {oversized}"
        );
        let mut shared_ids = Vec::new();
        for index in 0..64 {
            shared_ids.push(json!({"id":format!("s{index}"),"kind":"recent"}));
        }
        let wide = data(ffi(
            &json!({"scopes":shared_ids,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let wide_token = wide["leaseToken"].as_str().unwrap().to_owned();
        // Consumer admission: all 64 public IDs admitted over one source
        // generation (single shared capture charged once), and a sample of
        // aliases each serve the same recent trace.
        assert_eq!(wide["scopes"].as_array().unwrap().len(), 64);
        let generation = wide["scopes"][0]["sourceGeneration"].clone();
        assert!(
            wide["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .all(|scope| scope["sourceGeneration"] == generation),
            "64 same-producer IDs must share one capture: {wide}"
        );
        for sample in ["s0", "s31", "s63"] {
            assert!(
                !query_at(&wide_token, sample, "trace", now)
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "alias {sample} must serve the shared trace"
            );
        }
        release(&wide_token);

        // Quota fan-out: one recent plus 33 primary-quota aliases is one
        // producer, but naive per-ID registration would mint 33 window
        // tokens and evict the first at the 32-lease cap. Every returned
        // token must run an actual window query — proving none expired —
        // not merely compare strings or pointers.
        let mut fanout = vec![json!({"id":"live","kind":"recent"})];
        for index in 0..33 {
            fanout.push(json!({"id":format!("q{index}"),"kind":"quota"}));
        }
        let fanned = data(ffi(
            &json!({"scopes":fanout,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let fanned_scopes = fanned["scopes"].as_array().unwrap().clone();
        assert_eq!(fanned_scopes.len(), 34);
        for scope in &fanned_scopes {
            if scope["id"] == "live" {
                continue;
            }
            let token = scope["quotaSnapshotToken"].as_str().unwrap();
            let used = ffi(
                &json!({"accountKey":null,"fromMs":timestamp,"untilMs":now,
                    "snapshotToken":token}),
                crate::tb_window_usage,
            );
            assert_eq!(used["ok"], true, "quota token expired on first use: {used}");
        }
        let fanned_token = fanned["leaseToken"].as_str().unwrap().to_owned();
        release(&fanned_token);
    }
    #[test]
    fn hinted_sync_matches_authoritative_outputs_and_never_hides_errors() {
        if std::env::var_os("SYRTIS_LOCAL_HINTS_CHILD").is_none() {
            let root = tempfile::TempDir::new().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "local_usage::tests::hinted_sync_matches_authoritative_outputs_and_never_hides_errors", "--nocapture"])
                .env_clear().env("SYRTIS_LOCAL_HINTS_CHILD","1")
                .env("HOME",root.path()).env("TOKSCALE_CONFIG_DIR",root.path().join("config"))
                .env("XDG_CONFIG_HOME",root.path().join("xdg-config"))
                .env("XDG_DATA_HOME",root.path().join("xdg-data"))
                .env("APPDATA",root.path().join("roaming"))
                .env("LOCALAPPDATA",root.path().join("local"))
                .env("TOKSCALE_PRICING_CACHE_ONLY","1").env("TZ","UTC");
            for key in ["SystemRoot", "SystemDrive"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os("HOME").unwrap());
        let source = root.join(".claude/projects/hints/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let now = now_ms();
        let timestamp = now.saturating_sub(30_000);
        write_usage(&source, timestamp, 10);
        let exact_scope = json!([{"id":"claude","kind":"report","clients":["claude"]}]);
        let url = reqwest::Url::from_file_path(&source).unwrap().to_string();
        // Baseline capture, then an append consumed through the hinted path.
        let forced = data(ffi(
            &json!({"scopes":exact_scope,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let forced_token = forced["leaseToken"].as_str().unwrap().to_owned();
        assert_eq!(
            query_at(&forced_token, "claude", "graph", now)["summary"]["totalTokens"],
            38
        );
        write_usage(&source, timestamp, 50);
        let hinted = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":forced_token,
                "verifyTopology":false,"changedFileURLs":[url]}),
            crate::tb_local_usage_sync,
        ));
        let hinted_token = hinted["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(hinted_token, forced_token);
        assert_eq!(hinted["changed"], true);
        assert_eq!(
            query_at(&hinted_token, "claude", "graph", now)["summary"]["totalTokens"],
            78
        );
        // Authoritative control on a twin append must agree exactly.
        let authoritatively = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":hinted_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        assert_eq!(authoritatively["leaseToken"], hinted_token);
        assert_eq!(authoritatively["changed"], false);
        write_usage(&source, timestamp, 90);
        let authoritative = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":hinted_token,"force":true}),
            crate::tb_local_usage_sync,
        ));
        let authoritative_token = authoritative["leaseToken"].as_str().unwrap().to_owned();
        assert_eq!(
            query_at(&authoritative_token, "claude", "graph", now)["summary"]["totalTokens"],
            118
        );
        // Each write replaces the single fixture row, so the message count
        // stays 1 while the token total tracks the latest input.
        assert_eq!(
            query_at(&authoritative_token, "claude", "models", now)["totalMessages"],
            1
        );
        assert_eq!(
            ffi(
                &json!({"leaseToken":hinted_token,"scopeId":"claude","kind":"models","nowMs":now}),
                crate::tb_local_usage_query,
            )["ok"],
            false,
            "replacement must reject the previous generation"
        );
        // Malformed and unknown hints fall back to the authoritative scan and
        // never hide a real change or error.
        write_usage(&source, timestamp, 130);
        let malformed = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":authoritative_token,
                "verifyTopology":false,"changedFileURLs":["not a url", "/tmp/unknown.jsonl"]}),
            crate::tb_local_usage_sync,
        ));
        let malformed_token = malformed["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(malformed_token, authoritative_token);
        assert_eq!(
            query_at(&malformed_token, "claude", "graph", now)["summary"]["totalTokens"],
            158
        );
        // Dependency drift forces the authoritative fallback path: a new Claude
        // `.meta.json` sibling dirties a known baseline row, so the hinted
        // primary must not take the narrowed plan even though the primary
        // itself is unchanged and totals stay put.
        let sibling = source.parent().unwrap().join("session.meta.json");
        std::fs::write(&sibling, "{}").unwrap();
        let drifted = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":malformed_token,
                "verifyTopology":false,"changedFileURLs":[url]}),
            crate::tb_local_usage_sync,
        ));
        let drifted_token = drifted["leaseToken"].as_str().unwrap().to_owned();
        // No token/changed claim: identical content may legitimately reuse the
        // snapshot. The totals assertion below proves no loss or error.
        assert_eq!(
            query_at(&drifted_token, "claude", "graph", now)["summary"]["totalTokens"],
            158
        );
        std::fs::remove_file(&sibling).unwrap();
        // Replacement (delete + recreate with new content) must not take the
        // targeted path: the stale hint observes a new identity and falls back
        // to the authoritative scan, which discovers the recreated file.
        std::fs::remove_file(&source).unwrap();
        write_usage(&source, timestamp, 170);
        let stale_hint = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":drifted_token,
                "verifyTopology":false,"changedFileURLs":[url]}),
            crate::tb_local_usage_sync,
        ));
        let replaced_token = stale_hint["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(replaced_token, drifted_token);
        assert_eq!(
            query_at(&replaced_token, "claude", "graph", now)["summary"]["totalTokens"],
            198,
            "replacement must be rediscovered with new content, never trusted by hint"
        );
        // Unhinted change is still discovered by the later full check: append
        // without any hint, then verify the periodic scan picks it up.
        write_usage(&source, timestamp, 200);
        let unhinted = data(ffi(
            &json!({"scopes":exact_scope,"previousLeaseToken":replaced_token,"verifyTopology":false}),
            crate::tb_local_usage_sync,
        ));
        let unhinted_token = unhinted["leaseToken"].as_str().unwrap().to_owned();
        assert_ne!(unhinted_token, replaced_token);
        assert_eq!(
            query_at(&unhinted_token, "claude", "graph", now)["summary"]["totalTokens"],
            228
        );
        release(&forced_token);
        release(&hinted_token);
        release(&authoritative_token);
        release(&malformed_token);
        release(&drifted_token);
        release(&unhinted_token);
    }

    fn stable_historical_payload(mut value: Value) -> Value {
        if let Some(meta) = value.get_mut("meta").and_then(Value::as_object_mut) {
            meta.remove("generatedAt");
            meta.remove("processingTimeMs");
        }
        value.as_object_mut().unwrap().remove("processingTimeMs");
        value
    }

    #[test]
    fn ordinary_report_checkpoints_survive_restart_and_changed_capture() {
        let Some(phase) = std::env::var_os("SYRTIS_CHECKPOINT_RESTART_PHASE") else {
            let root = tempfile::TempDir::new().unwrap();
            for phase in ["seed", "append", "reopen"] {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command.args(["--exact", "local_usage::tests::ordinary_report_checkpoints_survive_restart_and_changed_capture", "--nocapture"])
                    .env_clear()
                    .env("SYRTIS_CHECKPOINT_RESTART_PHASE", phase)
                    .env("HOME", root.path())
                    .env("SYRTIS_CONFIG_DIR", root.path().join("config"))
                    .env("SYRTIS_USAGE_DIR", root.path().join("usage"))
                    .env("XDG_CONFIG_HOME", root.path().join("xdg-config"))
                    .env("XDG_DATA_HOME", root.path().join("xdg-data"))
                    .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
                    .env("TZ", "UTC");
                for key in ["SystemRoot", "SystemDrive"] {
                    if let Some(value) = std::env::var_os(key) {
                        command.env(key, value);
                    }
                }
                let output = command.output().unwrap();
                assert!(
                    output.status.success(),
                    "{phase}\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let phase = phase.to_str().unwrap();
        let root = PathBuf::from(std::env::var_os("HOME").unwrap());
        let directory = root.join(".claude/projects/checkpoint");
        std::fs::create_dir_all(&directory).unwrap();
        let row = |index: usize| {
            let timestamp = chrono::DateTime::from_timestamp_millis(
                1_735_689_600_000 + index as i64 * 3_600_000,
            )
            .unwrap()
            .to_rfc3339();
            json!({"type":"assistant","timestamp":timestamp,"requestId":format!("request-{index}"),
                "message":{"id":format!("message-{index}"),"model":"claude-sonnet-4",
                    "usage":{"input_tokens":index as i64 % 13+1,"output_tokens":3,
                        "cache_read_input_tokens":2,"cache_creation_input_tokens":1}}})
            .to_string()
                + "\n"
        };
        if phase == "seed" {
            use std::io::Write;
            let mut history = std::io::BufWriter::new(
                std::fs::File::create(directory.join("a-history.jsonl")).unwrap(),
            );
            for index in 0..2000 {
                history.write_all(row(index).as_bytes()).unwrap();
            }
            history.flush().unwrap();
            std::fs::write(directory.join("z-current.jsonl"), row(2000)).unwrap();
            let cache = root.join("config/cache");
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::write(
                cache.join("pricing-litellm.json"),
                json!({
                    "timestamp":now_ms()/1000,
                    "data":{"claude-sonnet-4":{"input_cost_per_token":0.013,
                        "output_cost_per_token":0.031,"cache_read_input_token_cost":0.001,
                        "cache_creation_input_token_cost":0.003}}
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                cache.join("pricing-openrouter.json"),
                json!({
                    "timestamp":now_ms()/1000,"data":{}
                })
                .to_string(),
            )
            .unwrap();
        } else if phase == "append" {
            std::fs::write(directory.join("z-current.jsonl"), row(2000) + &row(2001)).unwrap();
        }
        let sync = data(ffi(
            &json!({"scopes":[{"id":"report","kind":"report","clients":["claude"]}]}),
            crate::tb_local_usage_sync,
        ));
        let token = sync["leaseToken"].as_str().unwrap();
        let mut reports = serde_json::Map::new();
        for kind in ["graph", "models", "hourly", "agents"] {
            reports.insert(
                kind.into(),
                stable_historical_payload(query_at(token, "report", kind, now_ms())),
            );
        }
        let database =
            rusqlite::Connection::open(root.join("usage/source-message-cache.sqlite3")).unwrap();
        let checkpoints: i64 = database
            .query_row(
                "SELECT COUNT(*) FROM report_checkpoints WHERE row_count=?1",
                [if phase == "seed" { 2001 } else { 2002 }],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            checkpoints, 4,
            "all four ordinary kinds must persist exact producing folds"
        );
        if phase == "seed" {
            std::fs::write(
                root.join("seed-generation.json"),
                sync["scopes"][0]["sourceGeneration"].to_string(),
            )
            .unwrap();
        } else if phase == "append" {
            let previous: Value =
                serde_json::from_slice(&std::fs::read(root.join("seed-generation.json")).unwrap())
                    .unwrap();
            assert_ne!(previous, sync["scopes"][0]["sourceGeneration"]);
            let prior: i64 = database
                .query_row(
                    "SELECT COUNT(*) FROM report_checkpoints WHERE row_count=2001",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                prior, 4,
                "cross-generation prefix states must remain reachable after restart"
            );
            // Evict only disposable compact checkpoints to force the complete
            // historical oracle on the identical captured lease and pricing.
            database
                .execute("DELETE FROM report_checkpoints", [])
                .unwrap();
            for kind in ["graph", "models", "hourly", "agents"] {
                assert_eq!(
                    reports[kind],
                    stable_historical_payload(query_at(token, "report", kind, now_ms())),
                    "{kind}: full versus incremental payload differs"
                );
            }
            std::fs::write(
                root.join("append-reports.json"),
                serde_json::to_vec(&reports).unwrap(),
            )
            .unwrap();
        } else {
            let expected: Value =
                serde_json::from_slice(&std::fs::read(root.join("append-reports.json")).unwrap())
                    .unwrap();
            assert_eq!(
                Value::Object(reports),
                expected,
                "restarted exact pricing/query identity must preserve every payload field"
            );
        }
        release(token);
    }
}
