//! Coarse release-mode regression budgets, not a microbenchmark or a live-data claim.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};
use syrtis_core::{
    lean_local_graph_from_snapshot, synchronize_local_usage, try_synchronize_local_usage_sources,
    visit_local_usage_snapshot, GraphPricingMode, LocalParseOptions, LocalUsageQuery,
    LocalUsageSelection, LocalUsageSnapshot, ReportOptions, ResolvedLocalSourceContext,
    ScannerSettings,
};

const ROWS: usize = 4096;
const BASE: i64 = 1_767_225_600_000;

fn append(path: &Path, index: usize) {
    let timestamp = chrono::DateTime::from_timestamp_millis(BASE + index as i64 * 1000).unwrap();
    let row = serde_json::json!({
        "type": "assistant", "timestamp": timestamp.to_rfc3339(),
        "requestId": format!("request-{index}"),
        "message": {"id": format!("message-{index}"), "model": "claude-sonnet-4-6",
            "usage": {"input_tokens": 10, "output_tokens": 2,
                "cache_read_input_tokens": 3, "cache_creation_input_tokens": 5}}
    });
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap(),
        "{row}"
    )
    .unwrap();
}

fn totals(snapshot: &LocalUsageSnapshot, query: LocalUsageQuery) -> (usize, i64) {
    let mut result = (0, 0);
    visit_local_usage_snapshot(snapshot, query, &|_| true, &mut |message| {
        result.0 += 1;
        result.1 += message.tokens.total();
    })
    .unwrap();
    result
}

fn elapsed(start: Instant, seconds: u64, phase: &str) -> u128 {
    let duration = start.elapsed();
    println!(
        "performance {phase}: {} ms (budget {seconds} s)",
        duration.as_millis()
    );
    assert!(
        duration < Duration::from_secs(seconds),
        "{phase} exceeded coarse regression budget: {duration:?}"
    );
    duration.as_millis()
}

#[test]
#[ignore = "optimized synthetic workload; run with --release --ignored --nocapture"]
fn usage_pipeline_performance() {
    // A child process prevents developer overrides, legacy stores, or parallel
    // tests from turning the synthetic workload into a live-data scan.
    if std::env::var_os("SYRTIS_PERFORMANCE_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        let tmp = root.path().join("tmp");
        fs::create_dir(&tmp).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "usage_pipeline_performance",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("SYRTIS_PERFORMANCE_CHILD", "1")
            .env("HOME", root.path())
            .env("SYRTIS_CONFIG_DIR", root.path().join("config"))
            .env("SYRTIS_USAGE_DIR", root.path().join("usage"))
            .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
            .env("RAYON_NUM_THREADS", "4")
            .env("TZ", "UTC")
            .env("TMPDIR", &tmp)
            .env("TEMP", &tmp)
            .env("TMP", &tmp);
        for key in ["SystemRoot", "SystemDrive"] {
            if let Some(value) = std::env::var_os(key) {
                child.env(key, value);
            }
        }
        assert!(
            child.status().unwrap().success(),
            "synthetic performance child failed"
        );
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let sources = home.join(".claude/projects/performance");
    fs::create_dir_all(&sources).unwrap();
    for index in 0..ROWS {
        append(&sources.join(format!("{}.jsonl", index / 64)), index);
    }
    let source = sources.join(format!("{}.jsonl", (ROWS - 1) / 64));
    let context =
        ResolvedLocalSourceContext::capture(Some(home.clone()), false, ScannerSettings::default())
            .unwrap();
    let options = || LocalParseOptions {
        clients: Some(vec!["claude".into()]),
        use_env_roots: false,
        ..Default::default()
    };
    let report = || ReportOptions {
        clients: Some(vec!["claude".into()]),
        use_env_roots: false,
        ..Default::default()
    };
    let start = Instant::now();
    let cold = synchronize_local_usage(&context, options(), LocalUsageSelection::Report).unwrap();
    assert_eq!(
        totals(&cold, LocalUsageQuery::default()),
        (ROWS, ROWS as i64 * 20)
    );
    let cold_ms = elapsed(start, 30, "cold_sync_and_read");

    let start = Instant::now();
    let warm = synchronize_local_usage(&context, options(), LocalUsageSelection::Report).unwrap();
    for _ in 0..3 {
        let graph =
            lean_local_graph_from_snapshot(&warm, report(), None, GraphPricingMode::LocalOnly)
                .unwrap()
                .into_graph();
        assert_eq!(graph.summary.total_tokens, ROWS as i64 * 20);
    }
    let warm_ms = elapsed(start, 15, "warm_sync_and_three_graphs");

    let recent =
        synchronize_local_usage(&context, options(), LocalUsageSelection::LocalClients).unwrap();
    let query = LocalUsageQuery {
        from_ms: Some(BASE + (ROWS - 64) as i64 * 1000),
        until_ms: Some(BASE + ROWS as i64 * 1000),
        hydrate_opencode_aliases: false,
        ..Default::default()
    };
    let start = Instant::now();
    for _ in 0..32 {
        assert_eq!(totals(&recent, query), (64, 1280));
    }
    let recent_ms = elapsed(start, 15, "thirty_two_recent_queries");

    append(&source, ROWS);
    let start = Instant::now();
    let updated = try_synchronize_local_usage_sources(
        &context,
        options(),
        LocalUsageSelection::Report,
        &warm,
        &[source],
    )
    .unwrap()
    .expect("existing Claude source must support targeted synchronization");
    assert_eq!(
        totals(&updated, LocalUsageQuery::default()),
        (ROWS + 1, (ROWS as i64 + 1) * 20)
    );
    let targeted_ms = elapsed(start, 15, "targeted_append_and_read");
    println!(
        "{}",
        serde_json::json!({"rows": ROWS, "files": ROWS / 64,
        "cold_ms": cold_ms, "warm_ms": warm_ms, "recent_32_ms": recent_ms,
        "targeted_ms": targeted_ms})
    );
}
