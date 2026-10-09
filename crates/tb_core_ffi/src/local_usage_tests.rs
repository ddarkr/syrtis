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
}
