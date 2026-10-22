//! Live rate/trace folds over a pinned disk-store snapshot. Clock-only reads
//! never rediscover or parse sources; no historical event vector is retained.

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const EVENT_WINDOW_SECS: i64 = 3600;

#[derive(Debug, Clone, Serialize)]
pub struct TraceBucket {
    pub client: String,
    pub agent: String,
    pub model: String,
    pub tokens: i64,
    pub messages: u32,
    pub tokens_per_min: f32,
}

#[derive(Serialize)]
pub(crate) struct TraceRate {
    trace: Vec<TraceBucket>,
    rate: f32,
}

type TraceGroups = HashMap<(String, String, String), (i64, u32)>;

struct TailSnapshot {
    snapshot: syrtis_core::LocalUsageSnapshot,
    registry_generation: u64,
    cutoff: i64,
}

pub struct UsageTailer {
    snapshot: Mutex<Option<TailSnapshot>>,
}

impl UsageTailer {
    pub(crate) fn invalidate(&self) {
        *self.snapshot.lock() = None;
    }
    pub fn new() -> Self {
        Self {
            snapshot: Mutex::new(None),
        }
    }

    pub fn tick(&self) -> Result<(), String> {
        let _local_usage = crate::local_usage_guard();
        self.tick_with_options(crate::LocalSourceContext::current().parse_options(None, None))
    }

    fn tick_with_options(&self, options: syrtis_core::LocalParseOptions) -> Result<(), String> {
        let registry_generation = crate::root_generation();
        let context = syrtis_core::ResolvedLocalSourceContext::capture(
            options.home_dir.as_ref().map(PathBuf::from),
            options.use_env_roots,
            options.scanner_settings.clone(),
        )
        .map_err(|_| "live usage source context unavailable".to_string())?;
        let snapshot = syrtis_core::synchronize_local_usage(
            &context,
            options,
            syrtis_core::LocalUsageSelection::LocalClients,
        )?;
        let mut current = self.snapshot.lock();
        if registry_generation != crate::root_generation() {
            return Err("live usage scope changed; reissue".into());
        }
        *current = Some(TailSnapshot {
            snapshot,
            registry_generation,
            cutoff: now_ms().saturating_sub(EVENT_WINDOW_SECS * 1000),
        });
        Ok(())
    }

    fn with_snapshot<T>(
        &self,
        query: impl FnOnce(&syrtis_core::LocalUsageSnapshot, i64) -> Result<T, String>,
    ) -> Result<T, String> {
        let current = self.snapshot.lock();
        let current = current.as_ref().ok_or("live usage snapshot unavailable")?;
        if current.registry_generation != crate::root_generation() {
            return Err("live usage scope changed; reissue".into());
        }
        let result = query(&current.snapshot, current.cutoff)?;
        if current.registry_generation != crate::root_generation() {
            return Err("live usage scope changed; reissue".into());
        }
        Ok(result)
    }

    pub fn rate_in_window(&self, window_secs: i64) -> Result<f32, String> {
        self.rate_at(window_secs, now_ms())
    }

    fn rate_at(&self, window_secs: i64, now: i64) -> Result<f32, String> {
        if window_secs <= 0 {
            return Ok(0.0);
        }
        self.with_snapshot(|snapshot, cutoff| {
            rate_from_snapshot(snapshot, cutoff, window_secs, now)
        })
    }

    pub fn trace(&self, window_secs: i64) -> Result<Vec<TraceBucket>, String> {
        self.trace_at(window_secs, now_ms())
    }

    fn trace_at(&self, window_secs: i64, now: i64) -> Result<Vec<TraceBucket>, String> {
        if window_secs <= 0 {
            return Ok(Vec::new());
        }
        self.with_snapshot(|snapshot, cutoff| {
            trace_from_snapshot(snapshot, cutoff, window_secs, now)
        })
    }
}

fn visit_snapshot(
    snapshot: &syrtis_core::LocalUsageSnapshot,
    cutoff: i64,
    window_secs: i64,
    now: i64,
    sink: &mut impl FnMut(&syrtis_core::UnifiedMessage),
) -> Result<(), String> {
    let cutoff = now
        .saturating_sub(window_secs.saturating_mul(1000))
        .max(cutoff);
    syrtis_core::visit_local_usage_snapshot(
        snapshot,
        syrtis_core::LocalUsageQuery {
            from_ms: Some(cutoff),
            // The live clock is inclusive; later-dated rows are not live yet.
            // At i64::MAX no representable timestamp needs an upper bound.
            until_ms: now.checked_add(1),
            chronological: true,
            hydrate_opencode_aliases: false,
        },
        &|_| true,
        sink,
    )
}

pub(crate) fn rate_from_snapshot(
    snapshot: &syrtis_core::LocalUsageSnapshot,
    cutoff: i64,
    window_secs: i64,
    now: i64,
) -> Result<f32, String> {
    if window_secs <= 0 {
        return Ok(0.0);
    }
    let mut total = 0_i64;
    visit_snapshot(snapshot, cutoff, window_secs, now, &mut |message| {
        total = total.saturating_add(event_total(message));
    })?;
    Ok(total as f32 / (window_secs as f32 / 60.0))
}

pub(crate) fn trace_from_snapshot(
    snapshot: &syrtis_core::LocalUsageSnapshot,
    cutoff: i64,
    window_secs: i64,
    now: i64,
) -> Result<Vec<TraceBucket>, String> {
    if window_secs <= 0 {
        return Ok(Vec::new());
    }
    let mut groups = TraceGroups::new();
    visit_snapshot(snapshot, cutoff, window_secs, now, &mut |message| {
        add_trace(&mut groups, message, event_total(message));
    })?;
    Ok(finish_trace(groups, window_secs))
}

pub(crate) fn trace_rate_from_snapshot(
    snapshot: &syrtis_core::LocalUsageSnapshot,
    cutoff: i64,
    window_secs: i64,
    now: i64,
) -> Result<TraceRate, String> {
    if window_secs <= 0 {
        return Ok(TraceRate {
            trace: Vec::new(),
            rate: 0.0,
        });
    }
    let mut total = 0_i64;
    let mut groups = TraceGroups::new();
    visit_snapshot(snapshot, cutoff, window_secs, now, &mut |message| {
        add_trace_rate(&mut total, &mut groups, message);
    })?;
    Ok(TraceRate {
        trace: finish_trace(groups, window_secs),
        rate: total as f32 / (window_secs as f32 / 60.0),
    })
}

fn add_trace_rate(
    total: &mut i64,
    groups: &mut TraceGroups,
    message: &syrtis_core::UnifiedMessage,
) {
    let tokens = event_total(message);
    // Keep the chronological scalar fold independent of per-key saturation.
    *total = total.saturating_add(tokens);
    add_trace(groups, message, tokens);
}

fn add_trace(groups: &mut TraceGroups, message: &syrtis_core::UnifiedMessage, tokens: i64) {
    let key = (
        message.client.clone(),
        message
            .agent
            .clone()
            .unwrap_or_else(|| message.client.clone()),
        message.model_id.clone(),
    );
    let slot = groups.entry(key).or_insert((0, 0));
    slot.0 = slot.0.saturating_add(tokens);
    slot.1 = slot.1.saturating_add(message.message_count.max(0) as u32);
}

fn finish_trace(groups: TraceGroups, window_secs: i64) -> Vec<TraceBucket> {
    let window_min = (window_secs as f32 / 60.0).max(1.0 / 60.0);
    let mut out: Vec<_> = groups
        .into_iter()
        .map(|((client, agent, model), (tokens, messages))| TraceBucket {
            client,
            agent,
            model,
            tokens,
            messages,
            tokens_per_min: tokens as f32 / window_min,
        })
        .collect();
    out.sort_by_key(|bucket| std::cmp::Reverse(bucket.tokens));
    out
}

fn event_total(message: &syrtis_core::UnifiedMessage) -> i64 {
    let tokens = &message.tokens;
    tokens
        .input
        .saturating_add(tokens.output)
        .saturating_add(tokens.reasoning)
        .saturating_add(tokens.cache_read)
        .saturating_add(tokens.cache_write)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_fold_preserves_signed_chronological_and_per_key_saturation() {
        let mut total = 0_i64;
        let mut groups = TraceGroups::new();
        let mut legacy_total = 0_i64;
        let mut legacy_groups = TraceGroups::new();
        for (model, input, output, messages) in [
            ("shared", i64::MAX, 1, i32::MAX),
            ("other", 17, 0, -1),
            ("shared", i64::MIN, 0, i32::MAX),
            ("shared", 0, 0, i32::MAX),
        ] {
            let mut message = syrtis_core::UnifiedMessage::new(
                "opencode",
                model,
                "openai",
                "session",
                0,
                syrtis_core::TokenBreakdown {
                    input,
                    output,
                    ..Default::default()
                },
                0.0,
            );
            message.message_count = messages;
            add_trace_rate(&mut total, &mut groups, &message);
            legacy_total = legacy_total.saturating_add(event_total(&message));
            add_trace(&mut legacy_groups, &message, event_total(&message));
        }
        assert_eq!(total, -1);
        assert_eq!(total, legacy_total);
        assert_eq!(groups, legacy_groups);
        assert_eq!(groups.values().map(|group| group.0).sum::<i64>(), 16);
        let trace = finish_trace(groups, 1);
        assert_eq!(trace[0].tokens, 17);
        assert_eq!(trace[0].messages, 0);
        assert_eq!(trace[1].tokens, -1);
        assert_eq!(trace[1].messages, u32::MAX);
        let legacy_window_min = (1.0_f32 / 60.0).max(1.0 / 60.0);
        assert_eq!(
            trace[1].tokens_per_min.to_bits(),
            (-1.0_f32 / legacy_window_min).to_bits()
        );
    }

    #[test]
    fn disk_tail_expires_without_source_reads_and_preserves_overflow_and_last_good() {
        if std::env::var_os("SYRTIS_DISK_TAIL_CHILD").is_none() {
            let isolated = tempfile::TempDir::new().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "usage_tail::tests::disk_tail_expires_without_source_reads_and_preserves_overflow_and_last_good", "--nocapture"])
                .env_clear()
                .env("HOME", isolated.path())
                .env("TOKSCALE_CONFIG_DIR", isolated.path().join("config"))
                .env("XDG_CONFIG_HOME", isolated.path().join("xdg-config"))
                .env("XDG_DATA_HOME", isolated.path().join("data"))
                .env("TOKSCALE_PRICING_CACHE_ONLY", "1")
                .env("SYRTIS_DISK_TAIL_CHILD", "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "isolated live tail failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = tempfile::TempDir::new().unwrap();
        let directory = home
            .path()
            .join(".local/share/opencode/storage/message/session");
        std::fs::create_dir_all(&directory).unwrap();
        let now = now_ms();
        for (id, timestamp, input) in [
            ("first", now - 30_000, i64::MAX),
            ("second", now - 20_000, 17),
            ("old", now - EVENT_WINDOW_SECS * 1000 - 1, 1000),
        ] {
            std::fs::write(directory.join(format!("{id}.json")), serde_json::json!({
                "id":id, "sessionID":"session", "role":"assistant", "modelID":"gpt-4o",
                "providerID":"openai", "mode":"build", "time":{"created":timestamp},
                "tokens":{"input":input,"output":19,"reasoning":23,"cache":{"read":29,"write":31}}
            }).to_string()).unwrap();
        }
        let options = syrtis_core::LocalParseOptions {
            home_dir: Some(home.path().to_string_lossy().into_owned()),
            clients: Some(vec!["opencode".to_string()]),
            ..Default::default()
        };
        let tailer = UsageTailer::new();
        tailer.tick_with_options(options.clone()).unwrap();
        assert_eq!(tailer.rate_at(60, now).unwrap(), i64::MAX as f32);
        let trace = tailer.trace_at(60, now).unwrap();
        assert_eq!(trace[0].tokens, i64::MAX);
        assert_eq!(trace[0].messages, 2);
        // Delete the actual source: querying the published generation must not
        // discover that deletion or reparse. Time expiry is nevertheless exact.
        std::fs::remove_dir_all(&directory).unwrap();
        assert_eq!(tailer.trace_at(60, now + 35_000).unwrap()[0].tokens, 119);
        assert!(tailer.trace_at(60, now + 60_000).unwrap().is_empty());
        assert_eq!(tailer.rate_at(60, now + 60_000).unwrap(), 0.0);
        assert!(tailer.trace_at(0, now).unwrap().is_empty());
        assert_eq!(tailer.rate_at(0, now).unwrap(), 0.0);
        // An unreadable/corrupt discovered DB is not a successful empty sync.
        let database = home.path().join(".local/share/opencode/opencode.db");
        std::fs::write(database, b"not a sqlite database").unwrap();
        assert!(tailer.tick_with_options(options).is_err());
        assert_eq!(tailer.trace_at(60, now).unwrap()[0].tokens, i64::MAX);
        assert_eq!(tailer.trace_at(60, now).unwrap()[0].messages, 2);
    }
}
