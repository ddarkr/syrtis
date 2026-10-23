#![deny(clippy::all)]

mod aggregator;
mod cc_mirror;
pub mod clients;
pub mod disk_usage;
pub mod fs_atomic;
mod message_cache;
pub mod model_alias;
mod parser;
pub mod paths;
pub mod pricing;
mod provider_identity;
pub mod remote_report;
pub mod scanner;
pub mod sessionize;
pub mod sessions;
mod source_context;
mod source_watch;
mod store_query;
mod usage_migration;
mod usage_store;
pub use store_query::{
    SelectionMode as LocalUsageSelection, UsageQuery as LocalUsageQuery,
    UsageSnapshot as LocalUsageSnapshot,
};

pub use aggregator::*;
pub use clients::{ClientCounts, ClientDef, ClientId, PathRoot};
pub use model_alias::{
    clear_model_aliases, model_alias_generation, model_aliases,
    register_usage_data_invalidation_hook, set_model_aliases, snapshot_grouping_aliases,
    GroupingAliasSnapshot, ModelAliasMap,
};
pub use parser::*;
pub use pricing::{RemotePricingDiagnostic, RemotePricingSnapshot};
pub use remote_report::{
    aggregate_remote_usage_v1, RemoteAgentsRecordV1, RemoteGraphRecordV1, RemoteHourlyRecordV1,
    RemoteModelsRecordV1, RemoteUsageBundleV1, RemoteUsageError, RemoteUsageQueryV1,
    REMOTE_TZDB_REVISION,
};
pub use scanner::*;
pub use sessionize::{
    compute_daily_active_time, compute_time_metrics, sessionize, SessionInterval,
    SessionizeAccumulator, TimeMetrics, DEFAULT_IDLE_GAP_MS,
};
pub use sessions::{CostSource, UnifiedMessage};
pub use source_context::{
    RemoteSourceContextError, RemoteSourceContextV1, RemoteSourceFingerprintV1,
    RemoteSourceScopeTokenV1, ResolvedLocalSourceContext, SourceContextUnavailable,
};
pub use source_watch::{
    capture_local_source_watch_manifest, LocalSourceOwnedWriteRule, LocalSourceVerification,
    LocalSourceWatchManifest, LocalSourceWatchRoot,
};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Recover the compact source cache from its retained-history recovery data.
/// Call only after preserving the failed database and its WAL/SHM files offline.
/// Queries never recover automatically or discard their last-good generation.
pub fn recover_source_message_cache(cache_root: &Path) -> Result<(), String> {
    message_cache::recover_source_message_cache(cache_root)
}

/// Strip a CLIProxyAPI-style `(level)` reasoning-effort suffix from a model id.
///
/// Mirrors <https://help.router-for.me/configuration/thinking>: the proxy
/// strips the parentheses before routing, so for pricing lookups we treat the
/// suffix as cosmetic and resolve to the base model. Accepts the level set the
/// proxy documents (case-insensitive — callers pass the lowercased id):
/// `minimal`, `low`, `medium`, `high`, `xhigh`, `auto`, `none`. Numeric
/// thinking budgets are intentionally not handled here.
pub(crate) fn strip_parenthesized_reasoning_tier(model_id: &str) -> Option<&str> {
    let without_closing_paren = model_id.strip_suffix(')')?;
    let (base_model, tier) = without_closing_paren.rsplit_once('(')?;

    if base_model.is_empty() || base_model.trim() != base_model {
        return None;
    }

    if !matches!(
        tier,
        "minimal" | "low" | "medium" | "high" | "xhigh" | "auto" | "none"
    ) {
        return None;
    }

    Some(base_model)
}

/// Canonical model identity — the model id that leaves the machine.
///
/// This is [`normalize_syntactic`] with **no alias folding**: purely structural
/// canonicalization (lowercase, strip a `(reasoning-tier)` suffix, strip a
/// trailing `-YYYYMMDD` date, rewrite `.`→`-` inside claude version numbers, and
/// fold an `anthropic/claude-…` prefix). It never consults the user's
/// machine-local model aliases.
///
/// Every path that submits, uploads, exports as raw data, or persists a model id
/// MUST use this, not [`normalize_model_for_grouping`]. A machine-local alias
/// config must never rewrite the model identity persisted server-side, or usage
/// history would fragment and fork across a user's devices. Graph
/// `ClientContribution` keys also use this so export/raw identity stays stable.
pub fn canonical_model_id(model_id: &str) -> String {
    normalize_syntactic(model_id)
}

/// Local display/grouping model name: [`canonical_model_id`] plus the user's
/// configured model-alias fold. Every local report-grouping surface — the models
/// report, every `GroupBy`, monthly, and hourly — routes through this so name
/// variants fold uniformly for presentation.
///
/// The alias fold is **presentation only** and must never reach the
/// submit/upload/export/persist path (those use [`canonical_model_id`]), pricing
/// (which resolves the raw message `model_id`), or the message-cache key space.
/// With no aliases installed this is [`canonical_model_id`] plus the one
/// built-in rule in [`model_alias`] (Grok Build `grok-<version>-build` →
/// `grok-<version>`).
pub fn normalize_model_for_grouping(model_id: &str) -> String {
    model_alias::apply_global(normalize_syntactic(model_id))
}

/// Structural-only model-name normalization: lowercase, strip a
/// `(reasoning-tier)` suffix, strip a trailing `-YYYYMMDD` date, rewrite `.`→`-`
/// inside claude version numbers, and fold an `anthropic/claude-…` prefix.
///
/// This is the syntactic half of [`normalize_model_for_grouping`] /
/// [`canonical_model_id`]. It is also used by [`model_alias`] to normalize
/// configured alias keys and values into the same space, so a configured alias
/// matches its model regardless of case, dated suffix, or `.`-vs-`-` spelling.
pub(crate) fn normalize_syntactic(model_id: &str) -> String {
    let mut name = model_id.to_lowercase();

    if let Some(base_model) = strip_parenthesized_reasoning_tier(&name) {
        name = base_model.to_string();
    }
    if let Some((base, potential_date)) = name.rsplit_once('-') {
        if !base.is_empty()
            && potential_date.len() == 8
            && potential_date.bytes().all(|byte| byte.is_ascii_digit())
        {
            name = base.to_string();
        }
    }

    if name.contains("claude") {
        let chars: Vec<char> = name.chars().collect();
        let mut result = String::with_capacity(name.len());
        for i in 0..chars.len() {
            if chars[i] == '.'
                && i > 0
                && i < chars.len() - 1
                && chars[i - 1].is_ascii_digit()
                && chars[i + 1].is_ascii_digit()
            {
                result.push('-');
            } else {
                result.push(chars[i]);
            }
        }
        name = result;
    }

    if let Some(canonical) = normalize_anthropic_prefixed_claude_model(&name) {
        name = canonical;
    }

    name
}

fn normalize_anthropic_prefixed_claude_model(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("anthropic/claude-")?;
    let mut parts = rest.split('-');
    let major = parts.next()?;
    let minor = parts.next()?;
    let family = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    if !matches!(family, "opus" | "sonnet" | "haiku") {
        return None;
    }

    Some(format!("claude-{family}-{major}-{minor}"))
}

fn retain_for_requested_clients(
    client: &str,
    model_id: &str,
    provider_id: &str,
    requested: &HashSet<&str>,
) -> bool {
    requested.contains(client)
        || (requested.contains("claude") && client.starts_with("cc-mirror/"))
        || (requested.contains("synthetic")
            && sessions::synthetic::matches_synthetic_filter(client, model_id, provider_id))
}

#[cfg(test)]
fn prefer_copilot_otel_messages(
    mut otel_messages: Vec<UnifiedMessage>,
    desktop_messages: Vec<UnifiedMessage>,
) -> Vec<UnifiedMessage> {
    let otel_sessions: HashSet<String> = otel_messages
        .iter()
        .map(|message| message.session_id.clone())
        .collect();
    otel_messages.extend(
        desktop_messages
            .into_iter()
            .filter(|message| !otel_sessions.contains(message.session_id.as_str())),
    );
    otel_messages
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub enum GroupBy {
    Model,
    #[default]
    ClientModel,
    ClientProviderModel,
    WorkspaceModel,
    Session,
    ClientSession,
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupBy::Model => write!(f, "model"),
            GroupBy::ClientModel => write!(f, "client,model"),
            GroupBy::ClientProviderModel => write!(f, "client,provider,model"),
            GroupBy::WorkspaceModel => write!(f, "workspace,model"),
            GroupBy::Session => write!(f, "session,model"),
            GroupBy::ClientSession => write!(f, "client,session,model"),
        }
    }
}

impl std::str::FromStr for GroupBy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized: String = s.split(',').map(|p| p.trim()).collect::<Vec<_>>().join(",");
        match normalized.to_lowercase().as_str() {
            "model" => Ok(GroupBy::Model),
            "client,model" | "client-model" => Ok(GroupBy::ClientModel),
            "client,provider,model" | "client-provider-model" => Ok(GroupBy::ClientProviderModel),
            "workspace,model" | "workspace-model" => Ok(GroupBy::WorkspaceModel),
            "session" | "session,model" | "session-model" => Ok(GroupBy::Session),
            "client,session" | "client-session" | "client,session,model" | "client-session-model" => {
                Ok(GroupBy::ClientSession)
            }
            _ => Err(format!(
                "Invalid group-by value: '{}'. Valid options: model, client,model, client,provider,model, workspace,model, session,model, client,session,model",
                s
            )),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    /// The 1-hour-TTL portion of `cache_write`, which Anthropic bills at 2x
    /// base input where a 5-minute write is 1.25x.
    ///
    /// This is a **subset** of `cache_write`, never a bucket beside it:
    /// `cache_write` stays the whole write and the invariant
    /// `cache_write_1h <= cache_write` holds, which is why `total()` and
    /// every total sum ignore this field. Pricing derives the 5-minute
    /// portion by subtraction (`cache_write - cache_write_1h`) and reprices
    /// this one; a consumer that adds this to a total, or prices it without
    /// subtracting, double-counts those tokens.
    ///
    /// Zero means "no 1h portion reported", which covers both a turn that
    /// wrote none and a transcript predating the split. Both price the whole
    /// write at the 5-minute rate, as every turn did before the split existed.
    ///
    /// `default` because this type is also read from JSON, where a payload
    /// written before the split simply lacks the key and must still decode.
    /// It does nothing for bincode, whose positional layout is what
    /// `CACHE_FORMAT_VERSION` 4 and `mod format3` exist to handle.
    #[serde(default)]
    pub cache_write_1h: i64,
}

impl TokenBreakdown {
    pub fn total(&self) -> i64 {
        // saturating so clamped (i64::MAX) buckets from a corrupt source can't
        // overflow the sum.
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.reasoning)
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPerformance {
    #[serde(rename = "msPer1KTokens")]
    pub ms_per_1k_tokens: Option<f64>,
    pub total_duration_ms: i64,
    pub timed_tokens: i64,
    pub sample_count: i32,
    pub token_coverage: f64,
}

impl ModelPerformance {
    pub fn record_message(&mut self, token_total: i64, duration_ms: Option<i64>) {
        let Some(duration_ms) = duration_ms else {
            return;
        };
        if duration_ms <= 0 || token_total <= 0 {
            return;
        }

        self.total_duration_ms = self.total_duration_ms.saturating_add(duration_ms);
        self.timed_tokens = self.timed_tokens.saturating_add(token_total);
        self.sample_count = self.sample_count.saturating_add(1);
    }

    pub fn finalize(&mut self, total_tokens: i64) {
        self.ms_per_1k_tokens = if self.timed_tokens > 0 && self.total_duration_ms > 0 {
            Some(self.total_duration_ms as f64 * 1000.0 / self.timed_tokens as f64)
        } else {
            None
        };

        self.token_coverage = if total_tokens > 0 {
            (self.timed_tokens as f64 / total_tokens as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }

    pub fn from_totals(total_duration_ms: i64, timed_tokens: i64, sample_count: i32) -> Self {
        let mut performance = Self {
            total_duration_ms,
            timed_tokens,
            sample_count,
            ..Self::default()
        };
        performance.finalize(timed_tokens);
        performance
    }
}

#[derive(Debug, Clone)]
pub struct ParsedMessage {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub session_id: String,
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    pub timestamp: i64,
    pub date: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub duration_ms: Option<i64>,
    pub message_count: i32,
    pub agent: Option<String>,
}

pub struct ParsedMessages {
    pub messages: Vec<ParsedMessage>,
    pub counts: ClientCounts,
    pub processing_time_ms: u32,
}

impl Clone for ParsedMessages {
    fn clone(&self) -> Self {
        let mut counts = ClientCounts::new();
        for client in ClientId::iter() {
            counts.set(client, self.counts.get(client));
        }

        Self {
            messages: self.messages.clone(),
            counts,
            processing_time_ms: self.processing_time_ms,
        }
    }
}

impl std::fmt::Debug for ParsedMessages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ParsedMessages");
        debug.field("messages", &self.messages);
        for client in ClientId::iter() {
            debug.field(client.as_str(), &self.counts.get(client));
        }
        debug.field("processing_time_ms", &self.processing_time_ms);
        debug.finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocalParseOptions {
    pub home_dir: Option<String>,
    pub use_env_roots: bool,
    pub clients: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub year: Option<String>,
    /// Persistent scanner config loaded from `~/.config/tokscale/settings.json`.
    /// Defaults to empty when callers don't care about user-configured paths.
    pub scanner_settings: scanner::ScannerSettings,
    /// Skip parsing session sources whose newest relevant mtime (unix ms) is
    /// older than this. Lets high-frequency callers (live tails) avoid
    /// re-parsing an entire history when they only need recent messages —
    /// callers align it with `since`. SQLite lanes are either retained or use
    /// source-specific DB/WAL/dependency probes because WAL writes may not touch
    /// the main database mtime.
    pub modified_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteSourceUsageError {
    SourceUnavailable,
    InvalidQuery,
    IncompatibleTzdb,
    InvalidTimeZone,
    InvalidTimestamp,
    InvalidText,
    InvalidNumerator,
    LimitExceeded,
}

impl std::fmt::Display for RemoteSourceUsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SourceUnavailable => "source unavailable",
            Self::InvalidQuery => "invalid query",
            Self::IncompatibleTzdb => "incompatible tzdb",
            Self::InvalidTimeZone => "invalid timezone",
            Self::InvalidTimestamp => "invalid timestamp",
            Self::InvalidText => "invalid text",
            Self::InvalidNumerator => "invalid numerator",
            Self::LimitExceeded => "limit exceeded",
        })
    }
}

impl std::error::Error for RemoteSourceUsageError {}

impl From<remote_report::RemoteUsageError> for RemoteSourceUsageError {
    fn from(error: remote_report::RemoteUsageError) -> Self {
        match error {
            remote_report::RemoteUsageError::InvalidQuery => Self::InvalidQuery,
            remote_report::RemoteUsageError::IncompatibleTzdb => Self::IncompatibleTzdb,
            remote_report::RemoteUsageError::InvalidTimeZone => Self::InvalidTimeZone,
            remote_report::RemoteUsageError::InvalidTimestamp => Self::InvalidTimestamp,
            remote_report::RemoteUsageError::InvalidText => Self::InvalidText,
            remote_report::RemoteUsageError::InvalidNumerator => Self::InvalidNumerator,
            remote_report::RemoteUsageError::LimitExceeded => Self::LimitExceeded,
        }
    }
}

/// Fold the production streaming source lane into the remote four-page
/// bundle.  Source parsing, cache reads, per-client deduplication, and pricing
/// all stay in the existing streaming driver; only the final fold is remote
/// specific.
pub fn aggregate_remote_usage_from_source_v1(
    context: &RemoteSourceContextV1,
    query: &RemoteUsageQueryV1,
    pricing_snapshot: &RemotePricingSnapshot,
) -> Result<RemoteUsageBundleV1, RemoteSourceUsageError> {
    let mut fold =
        remote_report::RemoteUsageFold::new(query).map_err(RemoteSourceUsageError::from)?;
    // A separate gate rather than the fold's own: the accept callback and the
    // sink are handed to the scan together, so they cannot both borrow the
    // fold. Both are built from the same query.
    let gate =
        remote_report::RemoteMessageGate::new(query).map_err(RemoteSourceUsageError::from)?;
    // Scan lanes stay expanded — a `cc-mirror/*` id is only produced by the
    // Claude lane — while the gate admits only what can still be emitted.
    let clients = if query.clients.is_empty() {
        Vec::new()
    } else {
        split_report_client_filter(&ReportOptions {
            clients: Some(query.clients.clone()),
            ..Default::default()
        })
        .0
    };
    let mut source_error = None;
    let accept_remote_message = |message: &UnifiedMessage| gate.accepts(message);
    let mut sink = |message: &UnifiedMessage| {
        if source_error.is_none() {
            source_error = fold.push(message).err().map(Into::into);
        }
    };
    scan_messages_streaming_with_context(
        context.local(),
        &clients,
        pricing_snapshot.service(),
        None,
        &accept_remote_message,
        &mut sink,
    )
    .map_err(|_| RemoteSourceUsageError::SourceUnavailable)?;
    if let Some(error) = source_error {
        return Err(error);
    }
    fold.finish().map_err(Into::into)
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DailyTotals {
    pub tokens: i64,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ClientContribution {
    pub client: String,
    pub model_id: String,
    pub provider_id: String,
    pub tokens: TokenBreakdown,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DailyContribution {
    pub date: String,
    pub totals: DailyTotals,
    pub intensity: u8,
    pub token_breakdown: TokenBreakdown,
    pub clients: Vec<ClientContribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_time_ms: Option<i64>,
    /// User interaction turns for this day, keyed by exact client id.
    ///
    /// The turn markers already ride the deduped, priced message stream this
    /// fold consumes, so counting them here costs one increment per message and
    /// saves a consumer from running a second full report just to show per-day
    /// turns. The hourly report stays the source for hour-resolution turns;
    /// this serves day and month views.
    ///
    /// Keyed by client rather than folded into `totals` because a consumer that
    /// hides a client has to be able to subtract it, and a single daily total
    /// cannot be un-mixed. Kept out of `ClientContribution` because that is
    /// keyed by client *and* model, while a turn belongs to the client alone —
    /// splitting one turn across a day's models would invent data. `BTreeMap`
    /// keeps serialization deterministic.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turns_by_client: BTreeMap<String, i64>,
}

/// Per-session aggregate of token usage, cost, and timing — keyed on
/// `session_id` so downstream consumers can attribute cost to a specific
/// agent-CLI session rather than just a date or model rollup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SessionContribution {
    pub session_id: String,
    pub client: String,
    pub provider: String,
    pub model: String,
    pub totals: DailyTotals,
    pub token_breakdown: TokenBreakdown,
    pub clients: Vec<ClientContribution>,
    /// Earliest message timestamp (unix seconds) in the session.
    pub first_seen: i64,
    /// Latest message timestamp (unix seconds) in the session.
    pub last_seen: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct YearSummary {
    pub year: String,
    pub total_tokens: i64,
    pub total_cost: f64,
    pub range_start: String,
    pub range_end: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DataSummary {
    pub total_tokens: i64,
    pub total_cost: f64,
    pub total_days: i32,
    pub active_days: i32,
    pub average_per_day: f64,
    pub max_cost_in_single_day: f64,
    pub clients: Vec<String>,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GraphPricingMode {
    LocalOnly,
    BestEffort,
}

impl GraphPricingMode {
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::LocalOnly => "localOnly",
            Self::BestEffort => "bestEffort",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CostCoverage {
    Complete,
    Partial,
    None,
}

impl CostCoverage {
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphMetaContract {
    pub pricing_mode: GraphPricingMode,
    pub cost_coverage: CostCoverage,
}

impl Default for GraphMetaContract {
    fn default() -> Self {
        Self {
            pricing_mode: GraphPricingMode::BestEffort,
            cost_coverage: CostCoverage::Complete,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphMeta {
    pub generated_at: String,
    pub version: String,
    pub date_range_start: String,
    pub date_range_end: String,
    pub processing_time_ms: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphResult {
    pub meta: GraphMeta,
    pub summary: DataSummary,
    pub years: Vec<YearSummary>,
    pub contributions: Vec<DailyContribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_metrics: Option<sessionize::TimeMetrics>,
}

/// A graph plus the immutable per-call metadata contract that describes it.
/// The contract travels with the graph instead of through process or thread
/// state, so interleaved reports cannot change an already-produced result.
#[derive(Debug, Clone)]
pub struct GraphResultWithContract {
    graph: GraphResult,
    contract: GraphMetaContract,
}

impl GraphResultWithContract {
    pub fn graph(&self) -> &GraphResult {
        &self.graph
    }

    pub fn contract(&self) -> GraphMetaContract {
        self.contract
    }

    pub fn into_parts(self) -> (GraphResult, GraphMetaContract) {
        (self.graph, self.contract)
    }

    pub fn into_graph(self) -> GraphResult {
        self.graph
    }
}

#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    pub home_dir: Option<String>,
    pub use_env_roots: bool,
    pub clients: Option<Vec<String>>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub year: Option<String>,
    pub group_by: GroupBy,
    /// Persistent scanner config loaded from `~/.config/tokscale/settings.json`.
    /// Defaults to empty when callers don't care about user-configured paths.
    pub scanner_settings: scanner::ScannerSettings,
}

/// Unpruned producing lane options for pinned hourly/Agents report scopes.
/// Default scopes also serve graph/models; exact dynamic-client scopes keep
/// the existing two-level lane and per-message filter semantics.
pub fn local_report_scan_options(options: &ReportOptions) -> LocalParseOptions {
    let clients = report_scan_clients(options);
    LocalParseOptions {
        home_dir: options.home_dir.clone(),
        use_env_roots: options.use_env_roots,
        clients: Some(clients),
        scanner_settings: options.scanner_settings.clone(),
        ..Default::default()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelUsage {
    pub client: String,
    pub merged_clients: Option<String>,
    pub workspace_key: Option<String>,
    pub workspace_label: Option<String>,
    pub session_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub message_count: i32,
    pub cost: f64,
    pub performance: ModelPerformance,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonthlyUsage {
    pub month: String,
    pub models: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub message_count: i32,
    pub cost: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelReport {
    pub entries: Vec<ModelUsage>,
    pub total_input: i64,
    pub total_output: i64,
    pub total_cache_read: i64,
    pub total_cache_write: i64,
    pub total_messages: i32,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

const UNKNOWN_WORKSPACE_LABEL: &str = "Unknown workspace";
const UNKNOWN_WORKSPACE_GROUP_KEY: &str = "\0unknown-workspace";

#[derive(Debug, Clone, serde::Serialize)]
pub struct MonthlyReport {
    pub entries: Vec<MonthlyUsage>,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

/// Hourly usage entry for a single hour slot (e.g. "2026-03-23 14:00")
#[derive(Debug, Clone, serde::Serialize)]
pub struct HourlyUsage {
    pub hour: String,
    pub clients: Vec<String>,
    pub models: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub message_count: i32,
    /// Number of user interaction turns (user→assistant boundaries).
    pub turn_count: i32,
    pub reasoning: i64,
    pub cost: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HourlyReport {
    pub entries: Vec<HourlyUsage>,
    pub total_cost: f64,
    pub processing_time_ms: u32,
}

/// One raw message inside a `get_window_usage` window. No bucketing: the
/// consumer folds it however it needs, which is why this carries every field
/// a fold might want rather than the pre-aggregated shape `HourlyUsage` uses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowMessage {
    pub timestamp: i64,
    pub client: String,
    pub provider_id: String,
    pub model_id: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub cost: f64,
    pub is_turn_start: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowUsage {
    pub messages: Vec<WindowMessage>,
    /// Messages with no usable timestamp (`timestamp <= 0`). These cannot be
    /// placed inside `[from_ms, until_ms)` at all, so they are counted here
    /// instead of silently dropped — matching `HourlyUsage`'s convention that
    /// `timestamp > 0` gates the "has a real timestamp" case elsewhere in
    /// this file. Scoped to the whole corpus, not to the window, since an
    /// undated message's membership in the window is unknowable.
    pub undated_count: u32,
    pub processing_time_ms: u32,
}

pub fn get_home_dir_string(home_dir_option: &Option<String>) -> Result<String, String> {
    home_dir_option
        .as_ref()
        .filter(|home| !home.is_empty())
        .cloned()
        .or_else(|| paths::platform_home_dir().map(|path| path.to_string_lossy().into_owned()))
        .ok_or_else(|| {
            "HOME directory not specified and could not determine home directory".to_string()
        })
}

fn parse_kimi_source(path: &Path) -> Vec<UnifiedMessage> {
    if sessions::kimi::is_kimi_code_path(path) {
        sessions::kimi::parse_kimi_code_file(path)
    } else {
        sessions::kimi::parse_kimi_file(path)
    }
}

#[cfg(test)]
static RETAIN_OBSERVED_MESSAGES_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn parse_all_messages_with_pricing_with_env_strategy(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
    modified_after: Option<u64>,
) -> Vec<UnifiedMessage> {
    collect_local_unified_messages(
        home_dir,
        clients,
        pricing,
        use_env_roots,
        scanner_settings,
        modified_after,
    )
    .expect("fixture source ingestion succeeds")
}

fn collect_local_unified_messages(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
    modified_after: Option<u64>,
) -> Result<Vec<UnifiedMessage>, String> {
    let mut messages = Vec::new();
    scan_messages_disk(
        None,
        Path::new(home_dir),
        clients,
        pricing,
        use_env_roots,
        scanner_settings,
        modified_after,
        store_query::SelectionMode::UnifiedCompatibility,
        &|_| true,
        &mut |message| messages.push(message.clone()),
    )?;
    Ok(messages)
}

#[cfg(test)]
fn dedupe_latest_trae_messages(mut messages: Vec<UnifiedMessage>) -> Vec<UnifiedMessage> {
    let mut latest_by_session: HashMap<String, UnifiedMessage> = HashMap::new();

    for message in messages.drain(..) {
        let session_id = message.session_id.clone();
        match latest_by_session.get_mut(&session_id) {
            Some(existing) => {
                let should_replace = message.timestamp > existing.timestamp
                    || (message.timestamp == existing.timestamp
                        && message.dedup_key.as_ref().is_some_and(|key| {
                            existing
                                .dedup_key
                                .as_ref()
                                .is_none_or(|existing_key| key > existing_key)
                        }));
                if should_replace {
                    *existing = message;
                }
            }
            None => {
                let _ = latest_by_session.insert(session_id, message);
            }
        }
    }

    let mut deduped: Vec<UnifiedMessage> = latest_by_session.into_values().collect();
    deduped.sort_unstable_by(|a, b| {
        a.session_id
            .cmp(&b.session_id)
            .then_with(|| a.timestamp.cmp(&b.timestamp))
    });
    deduped
}

fn filter_unified_messages(
    messages: Vec<UnifiedMessage>,
    options: &LocalParseOptions,
) -> Vec<UnifiedMessage> {
    let mut filtered = messages;

    if let Some(year) = &options.year {
        let year_prefix = format!("{}-", year);
        filtered.retain(|m| m.date.starts_with(&year_prefix));
    }

    if let Some(since) = &options.since {
        filtered.retain(|m| m.date.as_str() >= since.as_str());
    }

    if let Some(until) = &options.until {
        filtered.retain(|m| m.date.as_str() <= until.as_str());
    }

    filtered
}

fn workspace_bucket(msg: &UnifiedMessage) -> (String, Option<String>, String) {
    match (&msg.workspace_key, &msg.workspace_label) {
        (Some(key), Some(label)) => (key.clone(), Some(key.clone()), label.clone()),
        (Some(key), None) => (
            key.clone(),
            Some(key.clone()),
            sessions::workspace_label_from_key(key)
                .unwrap_or_else(|| UNKNOWN_WORKSPACE_LABEL.to_string()),
        ),
        _ => (
            UNKNOWN_WORKSPACE_GROUP_KEY.to_string(),
            None,
            UNKNOWN_WORKSPACE_LABEL.to_string(),
        ),
    }
}

#[cfg(test)]
fn aggregate_model_usage_entries(
    messages: Vec<UnifiedMessage>,
    group_by: &GroupBy,
) -> Vec<ModelUsage> {
    aggregate_model_usage_entries_with(group_by, |fold| {
        for message in &messages {
            fold(message);
        }
        Ok(())
    })
    .expect("in-memory fixture fold succeeds")
}

#[cfg(test)]
fn aggregate_model_usage_entries_with(
    group_by: &GroupBy,
    visit: impl FnOnce(&mut dyn FnMut(&UnifiedMessage)) -> Result<(), String>,
) -> Result<Vec<ModelUsage>, String> {
    aggregate_model_usage_entries_fold(group_by, &|_| true, |_, fold| visit(fold), None)
}

fn aggregate_model_usage_entries_fold(
    group_by: &GroupBy,
    filter: &dyn Fn(&UnifiedMessage) -> bool,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
    cached: Option<&HistoricalReportFold<'_>>,
) -> Result<Vec<ModelUsage>, String> {
    let mut model_map: HashMap<String, ModelUsage> = HashMap::new();
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = cached
        .map(|cached| std::borrow::Cow::Borrowed(&cached.aliases))
        .unwrap_or_else(|| std::borrow::Cow::Owned(model_alias::snapshot_grouping_aliases()));

    let fold = |model_map: &mut HashMap<String, ModelUsage>, msg: &UnifiedMessage| {
        let normalized = aliases.fold(normalize_syntactic(&msg.model_id));
        let (workspace_group_key, workspace_key, workspace_label) =
            if *group_by == GroupBy::WorkspaceModel {
                workspace_bucket(msg)
            } else {
                (String::new(), None, String::new())
            };
        let key = match group_by {
            GroupBy::Model => normalized.clone(),
            GroupBy::ClientModel => format!("{}:{}", msg.client, normalized),
            GroupBy::ClientProviderModel => {
                format!("{}:{}:{}", msg.client, msg.provider_id, normalized)
            }
            GroupBy::WorkspaceModel => format!("{}:{}", workspace_group_key, normalized),
            GroupBy::Session => format!("{}:{}", msg.session_id, normalized),
            GroupBy::ClientSession => {
                format!("{}:{}:{}", msg.client, msg.session_id, normalized)
            }
        };
        let merge_clients = matches!(group_by, GroupBy::Model | GroupBy::WorkspaceModel);
        let session_grouped = matches!(group_by, GroupBy::Session | GroupBy::ClientSession);
        let entry = model_map.entry(key).or_insert_with(|| ModelUsage {
            client: msg.client.clone(),
            merged_clients: if merge_clients {
                Some(msg.client.clone())
            } else {
                None
            },
            workspace_key: if matches!(group_by, GroupBy::WorkspaceModel) {
                workspace_key.clone()
            } else {
                None
            },
            workspace_label: if matches!(group_by, GroupBy::WorkspaceModel) {
                Some(workspace_label.clone())
            } else {
                None
            },
            session_id: if session_grouped {
                Some(msg.session_id.clone())
            } else {
                None
            },
            model: normalized.clone(),
            provider: msg.provider_id.clone(),
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            message_count: 0,
            cost: 0.0,
            performance: ModelPerformance::default(),
        });

        if merge_clients {
            if !entry.client.split(", ").any(|s| s == msg.client) {
                entry.client = format!("{}, {}", entry.client, msg.client);
            }

            if let Some(merged_clients) = &mut entry.merged_clients {
                if !merged_clients.split(", ").any(|s| s == msg.client) {
                    *merged_clients = format!("{}, {}", merged_clients, msg.client);
                }
            }
        }

        if *group_by != GroupBy::ClientProviderModel
            && !entry.provider.split(", ").any(|p| p == msg.provider_id)
        {
            entry.provider = format!("{}, {}", entry.provider, msg.provider_id);
        }

        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold (matches the grand-total sum below).
        entry.input = entry.input.saturating_add(msg.tokens.input);
        entry.output = entry.output.saturating_add(msg.tokens.output);
        entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
        entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
        entry.reasoning = entry.reasoning.saturating_add(msg.tokens.reasoning);
        entry.message_count = entry.message_count.saturating_add(msg.message_count.max(0));
        entry.cost += msg.cost;
        entry
            .performance
            .record_message(positive_token_total(&msg.tokens), msg.duration_ms);
    };
    visit_report_state(&mut model_map, cached, filter, visit, fold)?;

    let mut entries: Vec<ModelUsage> = model_map
        .into_values()
        .map(|mut entry| {
            let total_tokens = entry
                .input
                .max(0)
                .saturating_add(entry.output.max(0))
                .saturating_add(entry.cache_read.max(0))
                .saturating_add(entry.cache_write.max(0))
                .saturating_add(entry.reasoning.max(0));
            entry.performance.finalize(total_tokens);
            let mut providers: Vec<&str> = entry.provider.split(", ").collect();
            providers.sort_unstable();
            providers.dedup();
            entry.provider = providers.join(", ");
            entry
        })
        .collect();
    entries.sort_by(|a, b| {
        let cost = match (a.cost.is_nan(), b.cost.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => b
                .cost
                .partial_cmp(&a.cost)
                .unwrap_or(std::cmp::Ordering::Equal),
        };
        cost.then_with(|| {
            (
                &a.client,
                &a.model,
                &a.provider,
                &a.workspace_key,
                &a.session_id,
            )
                .cmp(&(
                    &b.client,
                    &b.model,
                    &b.provider,
                    &b.workspace_key,
                    &b.session_id,
                ))
        })
    });

    Ok(entries)
}

fn positive_token_total(tokens: &TokenBreakdown) -> i64 {
    // saturating so multiple clamped (i64::MAX) buckets can't overflow the sum.
    tokens
        .input
        .max(0)
        .saturating_add(tokens.output.max(0))
        .saturating_add(tokens.cache_read.max(0))
        .saturating_add(tokens.cache_write.max(0))
        .saturating_add(tokens.reasoning.max(0))
}

/// Sum the (input, output, cache_read, cache_write) token fields across model
/// usage entries with saturating_add, so clamped (i64::MAX) entry buckets from a
/// corrupt source can't overflow the report-level totals (the entries are
/// already saturated per-field by aggregate_model_usage_entries).
fn model_report_token_totals(entries: &[ModelUsage]) -> (i64, i64, i64, i64) {
    entries.iter().fold(
        (0, 0, 0, 0),
        |(input, output, cache_read, cache_write), entry| {
            (
                input.saturating_add(entry.input),
                output.saturating_add(entry.output),
                cache_read.saturating_add(entry.cache_read),
                cache_write.saturating_add(entry.cache_write),
            )
        },
    )
}

/// Returns the effective client list for a report: uses the caller-supplied
/// list when present, or falls back to all known clients + "synthetic".
fn resolve_report_clients(options: &ReportOptions) -> Vec<String> {
    options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::ALL
            .iter()
            .map(|c| c.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    })
}

/// Two-level client filter for the streaming hourly/agents reports (TokenBar
/// local patch). `scan_messages_streaming` selects scanner LANES from the
/// client list, but some visible client ids are not lanes of their own: a
/// `cc-mirror/<variant>` id is produced *during Claude-lane parsing* (#659), so
/// requesting it alone would enable no lane (`ClientId::from_str` returns None)
/// and the report would come back empty for a client that Daily/Models still
/// show. Split the request into:
/// - the lanes to scan — each `cc-mirror/*` maps to its producing `claude`
///   lane (the Claude scanner discovers `.cc-mirror/*/config/projects`), every
///   other id maps to itself; and
/// - an optional EXACT client-id set the aggregation keeps. Unlike the scan's
///   built-in `retain_for_requested_clients`, requesting `claude` here does NOT
///   sweep in the distinct `cc-mirror/*` variants: the graph/model/daily
///   payloads fold by `msg.client`, surfacing each variant as its own client
///   id, so the hourly/agents slices must match that exact-id grouping. The
///   `synthetic` special-case is preserved (synthetic is a model/provider match,
///   not a literal client id). `None` = no client filter (every message),
///   preserving the all-clients behavior.
fn split_report_client_filter(options: &ReportOptions) -> (Vec<String>, Option<HashSet<String>>) {
    (
        report_scan_clients(options),
        options
            .clients
            .as_ref()
            .map(|requested| requested.iter().cloned().collect()),
    )
}

fn report_scan_clients(options: &ReportOptions) -> Vec<String> {
    let Some(requested) = options.clients.as_ref() else {
        return resolve_report_clients(options);
    };
    let mut lanes: Vec<String> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for id in requested {
        let lane = if id.starts_with("cc-mirror/") {
            "claude"
        } else {
            id.as_str()
        };
        if seen.insert(lane) {
            lanes.push(lane.to_owned());
        }
    }
    lanes
}

/// Exact per-message client gate for `split_report_client_filter`'s aggregation
/// half: keep a message iff its exact client id was requested, or it is a
/// synthetic match when `synthetic` was requested. `None` keeps every message.
pub(crate) fn report_message_client_passes(
    exact: &Option<HashSet<String>>,
    m: &UnifiedMessage,
) -> bool {
    match exact {
        None => true,
        Some(req) => {
            req.contains(m.client.as_str())
                || (req.contains("synthetic")
                    && sessions::synthetic::matches_synthetic_filter(
                        &m.client,
                        &m.model_id,
                        &m.provider_id,
                    ))
        }
    }
}

#[cfg(test)]
/// Cross-store OpenCode source identity. A migrated SQLite message can carry
/// both an embedded id and a v1 row/file fallback, so callers compare every
/// primary and alternate key at the same creation timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenCodeSourceIdentity {
    key: String,
    timestamp: i64,
}

#[cfg(test)]
impl OpenCodeSourceIdentity {
    fn all_from_message(message: &UnifiedMessage) -> Vec<Self> {
        let mut identities = Vec::new();
        let mut seen = HashSet::new();
        for key in message.dedup_key.iter().chain(message.dedup_aliases.iter()) {
            if !key.is_empty() && seen.insert(key.clone()) {
                identities.push(Self {
                    key: key.clone(),
                    timestamp: message.timestamp,
                });
            }
        }
        identities
    }
}

#[cfg(test)]
/// Logical OpenCode payload identity used after parser-local fork collapse.
///
/// Source keys are indexed separately because one migrated request can have
/// both an embedded id and a row/file fallback. Session and workspace remain
/// excluded because true fork copies move between both. Cost is excluded so a
/// provider-reported copy can still replace an estimated one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OpenCodePayloadIdentity {
    timestamp: i64,
    duration_ms: Option<i64>,
    model_id: String,
    provider_id: String,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    agent: Option<String>,
}

#[cfg(test)]
impl OpenCodePayloadIdentity {
    fn from_message(message: &UnifiedMessage) -> Self {
        Self {
            timestamp: message.timestamp,
            duration_ms: message.duration_ms,
            model_id: message.model_id.clone(),
            provider_id: message.provider_id.clone(),
            input: message.tokens.input,
            output: message.tokens.output,
            cache_read: message.tokens.cache_read,
            cache_write: message.tokens.cache_write,
            reasoning: message.tokens.reasoning,
            agent: message.agent.clone(),
        }
    }
}

#[cfg(test)]
fn opencode_identity_group(message: &UnifiedMessage) -> (bool, Vec<OpenCodeSourceIdentity>) {
    (
        message.cost_source == CostSource::ProviderReported,
        OpenCodeSourceIdentity::all_from_message(message),
    )
}

#[cfg(test)]
fn opencode_authoritative_sources(
    groups: impl IntoIterator<Item = (bool, Vec<OpenCodeSourceIdentity>)>,
) -> HashSet<OpenCodeSourceIdentity> {
    let groups: Vec<_> = groups.into_iter().collect();
    let mut authoritative = HashSet::new();
    for (is_provider_reported, sources) in &groups {
        if *is_provider_reported {
            authoritative.extend(sources.iter().cloned());
        }
    }
    loop {
        let previous_len = authoritative.len();
        for (_, sources) in &groups {
            if sources.iter().any(|source| authoritative.contains(source)) {
                authoritative.extend(sources.iter().cloned());
            }
        }
        if authoritative.len() == previous_len {
            return authoritative;
        }
    }
}

#[cfg(test)]
/// Applies one OpenCode source-priority snapshot without requiring sink
/// retraction. SQLite fork copies deduplicate only when a source key (primary or
/// alias) and the full payload identity both match; distinct embedded ids remain
/// separate even when every payload field collides.
struct OpenCodeSelection {
    authoritative_snapshot: HashSet<OpenCodeSourceIdentity>,
    seen_sqlite: HashMap<OpenCodeSourceIdentity, HashSet<OpenCodePayloadIdentity>>,
    emitted_sources: HashSet<OpenCodeSourceIdentity>,
    deferred_sqlite: Vec<Option<UnifiedMessage>>,
    deferred_sources: Vec<Vec<OpenCodeSourceIdentity>>,
    deferred_by_identity:
        HashMap<OpenCodeSourceIdentity, HashMap<OpenCodePayloadIdentity, Vec<usize>>>,
    deferred_by_source: HashMap<OpenCodeSourceIdentity, Vec<usize>>,
}

#[cfg(test)]
impl OpenCodeSelection {
    fn new(authoritative_snapshot: HashSet<OpenCodeSourceIdentity>) -> Self {
        Self {
            authoritative_snapshot,
            seen_sqlite: HashMap::new(),
            emitted_sources: HashSet::new(),
            deferred_sqlite: Vec::new(),
            deferred_sources: Vec::new(),
            deferred_by_identity: HashMap::new(),
            deferred_by_source: HashMap::new(),
        }
    }

    fn has_authority(&self, sources: &[OpenCodeSourceIdentity]) -> bool {
        sources
            .iter()
            .any(|source| self.authoritative_snapshot.contains(source))
    }

    fn has_emitted(&self, sources: &[OpenCodeSourceIdentity]) -> bool {
        sources
            .iter()
            .any(|source| self.emitted_sources.contains(source))
    }

    fn connected_deferred_sources(
        &self,
        sources: &[OpenCodeSourceIdentity],
    ) -> Vec<OpenCodeSourceIdentity> {
        let mut connected: HashSet<_> = sources.iter().cloned().collect();
        let mut pending: Vec<_> = sources.to_vec();
        while let Some(source) = pending.pop() {
            let Some(indices) = self.deferred_by_source.get(&source) else {
                continue;
            };
            for &index in indices {
                for alias in &self.deferred_sources[index] {
                    if connected.insert(alias.clone()) {
                        pending.push(alias.clone());
                    }
                }
            }
        }
        connected.into_iter().collect()
    }

    fn mark_emitted(&mut self, sources: &[OpenCodeSourceIdentity]) {
        let connected = self.connected_deferred_sources(sources);
        self.emitted_sources.extend(connected);
    }

    fn mark_sqlite_seen(
        &mut self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) -> bool {
        let duplicate = sources.iter().any(|source| {
            self.seen_sqlite
                .get(source)
                .is_some_and(|payloads| payloads.contains(payload))
        });
        for source in sources {
            self.seen_sqlite
                .entry(source.clone())
                .or_default()
                .insert(payload.clone());
        }
        duplicate
    }

    fn index_deferred(
        &mut self,
        index: usize,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) {
        for source in sources {
            if !self.deferred_sources[index].contains(source) {
                self.deferred_sources[index].push(source.clone());
            }
            let identity_indices = self
                .deferred_by_identity
                .entry(source.clone())
                .or_default()
                .entry(payload.clone())
                .or_default();
            if !identity_indices.contains(&index) {
                identity_indices.push(index);
            }
            let source_indices = self.deferred_by_source.entry(source.clone()).or_default();
            if !source_indices.contains(&index) {
                source_indices.push(index);
            }
        }
    }

    fn defer_sqlite(
        &mut self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
        message: UnifiedMessage,
    ) {
        let index = self.deferred_sqlite.len();
        self.deferred_sqlite.push(Some(message));
        self.deferred_sources.push(Vec::new());
        self.index_deferred(index, sources, payload);
    }

    fn first_active(&self, indices: &[usize]) -> Option<usize> {
        indices
            .iter()
            .copied()
            .find(|&index| self.deferred_sqlite[index].is_some())
    }

    fn find_deferred_identity(
        &self,
        sources: &[OpenCodeSourceIdentity],
        payload: &OpenCodePayloadIdentity,
    ) -> Option<usize> {
        sources.iter().find_map(|source| {
            self.deferred_by_identity
                .get(source)
                .and_then(|payloads| payloads.get(payload))
                .and_then(|indices| self.first_active(indices))
        })
    }

    fn find_deferred_source(&self, sources: &[OpenCodeSourceIdentity]) -> Option<usize> {
        sources.iter().find_map(|source| {
            self.deferred_by_source
                .get(source)
                .and_then(|indices| self.first_active(indices))
        })
    }

    fn take_deferred(&mut self, index: usize) -> Vec<OpenCodeSourceIdentity> {
        self.deferred_sqlite[index] = None;
        self.deferred_sources[index].clone()
    }

    fn select_sqlite(&mut self, message: UnifiedMessage) -> Option<UnifiedMessage> {
        let sources = OpenCodeSourceIdentity::all_from_message(&message);
        if sources.is_empty() {
            return Some(message);
        }
        let payload = OpenCodePayloadIdentity::from_message(&message);
        if self.mark_sqlite_seen(&sources, &payload) {
            if let Some(index) = self.find_deferred_identity(&sources, &payload) {
                self.index_deferred(index, &sources, &payload);
                if message.cost_source == CostSource::ProviderReported {
                    let deferred_sources = self.take_deferred(index);
                    self.mark_emitted(&deferred_sources);
                    self.mark_emitted(&sources);
                    return Some(message);
                }
                return None;
            }
            if self.has_emitted(&sources) {
                self.mark_emitted(&sources);
            }
            return None;
        }
        if self.has_emitted(&sources) {
            self.mark_emitted(&sources);
        }
        if self.has_authority(&sources) && message.cost_source != CostSource::ProviderReported {
            self.defer_sqlite(&sources, &payload, message);
            return None;
        }
        self.mark_emitted(&sources);
        Some(message)
    }

    /// `will_emit` reaching the selection is what lets a filtered-out
    /// authoritative JSON copy leave its deferred SQLite twin in place, so the
    /// twin is emitted instead — deliberate for a local report (see
    /// `test_opencode_streaming_selection_keeps_fallback_until_json_is_emitted`),
    /// but it means selection depends on the caller's filter. A remote
    /// producer passes `true` here and applies its own gate to the result,
    /// because it claims byte-parity with the pure fold over the same
    /// messages, and the pure fold has no such fallback.
    fn select_json(&mut self, message: UnifiedMessage, will_emit: bool) -> Option<UnifiedMessage> {
        let sources = OpenCodeSourceIdentity::all_from_message(&message);
        if sources.is_empty() {
            return will_emit.then_some(message);
        }
        if self.has_authority(&sources) {
            if message.cost_source != CostSource::ProviderReported || !will_emit {
                return None;
            }
            let payload = OpenCodePayloadIdentity::from_message(&message);
            let exact = self.find_deferred_identity(&sources, &payload);
            if exact.is_none() && self.has_emitted(&sources) {
                return None;
            }
            if let Some(index) = exact.or_else(|| self.find_deferred_source(&sources)) {
                let deferred_sources = self.take_deferred(index);
                self.mark_emitted(&deferred_sources);
            }
            self.mark_emitted(&sources);
            return Some(message);
        }
        if self.has_emitted(&sources) {
            return None;
        }
        self.mark_emitted(&sources);
        will_emit.then_some(message)
    }

    fn finish(self) -> impl Iterator<Item = UnifiedMessage> {
        self.deferred_sqlite.into_iter().flatten()
    }
}

fn scan_report_messages(
    context: Option<&ResolvedLocalSourceContext>,
    home: &str,
    clients: &[String],
    options: &ReportOptions,
    pricing: Option<&pricing::PricingService>,
    filter: &dyn Fn(&UnifiedMessage) -> bool,
    sink: &mut dyn FnMut(&UnifiedMessage),
) -> Result<(), String> {
    if let Some(context) = context {
        scan_messages_streaming_with_context(context, clients, pricing, None, &|m| filter(m), sink)
    } else {
        scan_messages_streaming(
            home,
            clients,
            pricing,
            options.use_env_roots,
            &options.scanner_settings,
            &|m| filter(m),
            sink,
        )
    }
}

fn validate_report_snapshot(
    snapshot: &LocalUsageSnapshot,
    clients: &[String],
) -> Result<(), String> {
    if snapshot.mode != LocalUsageSelection::Report
        || snapshot.selection_ignores_filter
        || snapshot.modified_after.is_some()
        || snapshot.clients != clients
    {
        return Err(
            "report requires an unpruned Report snapshot with the exact scan client scope".into(),
        );
    }
    Ok(())
}

/// The key and every cached calendar transform share these immutable TZif
/// rules. chrono::Local has a one-second thread-local refresh cache and cannot
/// safely be combined with a freshly captured timezone fingerprint.
struct CapturedReportTimezone {
    zone: jiff::tz::TimeZone,
    digest: [u8; 32],
}

impl CapturedReportTimezone {
    fn capture() -> Option<Self> {
        use sha2::{Digest, Sha256};
        use std::borrow::Cow;
        use std::io::Read;
        let requested = std::env::var("TZ").ok();
        if requested.as_deref() == Some("") {
            return Some(Self {
                zone: jiff::tz::TimeZone::UTC,
                digest: Sha256::digest(b"syrtis-report-empty-tz-utc-v1").into(),
            });
        }
        let name = requested.as_deref().unwrap_or("localtime");
        let localtime = name == "localtime";
        let name = name.strip_prefix(':').unwrap_or(name);
        let primary = if localtime {
            Some(Cow::Borrowed(std::path::Path::new("/etc/localtime")))
        } else if std::path::Path::new(name).is_absolute() {
            Some(Cow::Borrowed(std::path::Path::new(name)))
        } else {
            None
        };
        // The same file search order used by chrono's Unix resolver. Resolve
        // lazily instead of allocating all candidate paths on every query.
        let directories: &[&str] = if primary.is_some() {
            &[]
        } else {
            &[
                "/usr/share/zoneinfo",
                "/share/zoneinfo",
                "/etc/zoneinfo",
                "/usr/share/lib/zoneinfo",
            ]
        };
        for path in primary.into_iter().chain(
            directories
                .iter()
                .map(|directory| Cow::Owned(std::path::Path::new(directory).join(name))),
        ) {
            let Ok(file) = std::fs::File::open(&*path) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                return None;
            };
            if !metadata.is_file() || metadata.len() > 1024 * 1024 {
                return None;
            }
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            if file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err() {
                return None;
            }
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let zone = jiff::tz::TimeZone::tzif("captured-report", &bytes).ok()?;
            return Some(Self {
                zone,
                digest: Sha256::digest(&bytes).into(),
            });
        }
        // Custom POSIX rules/unresolved zones keep the authoritative complete
        // prepared fold. Never persist a state under an uncertain identity.
        None
    }

    fn date(&self, milliseconds: i64) -> String {
        let Ok(timestamp) = jiff::Timestamp::from_millisecond(milliseconds) else {
            return String::new();
        };
        let Some(datetime) = chrono::DateTime::from_timestamp_millis(milliseconds) else {
            return String::new();
        };
        let Some(offset) = chrono::FixedOffset::east_opt(self.zone.to_offset(timestamp).seconds())
        else {
            return String::new();
        };
        datetime
            .with_timezone(&offset)
            .format("%Y-%m-%d")
            .to_string()
    }

    fn hour(&self, message: &UnifiedMessage) -> String {
        if message.timestamp > 0 {
            // Match the existing hourly contract's second truncation exactly.
            let seconds = message.timestamp / 1000;
            if let (Ok(timestamp), Some(datetime)) = (
                jiff::Timestamp::from_second(seconds),
                chrono::DateTime::from_timestamp(seconds, 0),
            ) {
                if let Some(offset) =
                    chrono::FixedOffset::east_opt(self.zone.to_offset(timestamp).seconds())
                {
                    return datetime
                        .with_timezone(&offset)
                        .format("%Y-%m-%d %H:00")
                        .to_string();
                }
            }
        }
        format!("{} 00:00", message.date)
    }
}

/// The saved value is the actual sequential accumulator, not a partial sum.
/// A changed source generation does not enter its key: selected membership
/// establishes the longest reusable prefix before any payload is decoded.
struct HistoricalReportFold<'a> {
    query: store_query::HistoricalQuery<'a>,
    checkpoint: usage_store::ReportCheckpoint,
    snapshot: &'a LocalUsageSnapshot,
    pricing: Option<&'a pricing::PricingService>,
    aliases: model_alias::GroupingAliasSnapshot,
    timezone: Option<CapturedReportTimezone>,
}

impl<'a> HistoricalReportFold<'a> {
    fn new(
        snapshot: &'a LocalUsageSnapshot,
        options: &ReportOptions,
        pricing: Option<&'a pricing::PricingService>,
        kind: &str,
    ) -> Result<Self, String> {
        use sha2::{Digest, Sha256};
        let aliases = model_alias::snapshot_grouping_aliases();
        let mut hash = Sha256::new();
        // Bump the checkpoint revision when report arithmetic/grouping or
        // pricing-policy semantics change within the same package version.
        hash.update(b"syrtis-sequential-report-checkpoint-v2");
        hash.update(env!("CARGO_PKG_VERSION").as_bytes());
        hash.update(
            bincode::serialize(&(
                &snapshot.scope,
                kind,
                &options.clients,
                &options.year,
                &options.since,
                &options.until,
                &options.group_by,
            ))
            .map_err(|error| error.to_string())?,
        );
        hash.update(aliases.content_digest());
        if let Some(pricing) = pricing {
            hash.update([1]);
            hash.update(pricing.content_digest());
        } else {
            hash.update([0]);
        }
        let mut timezone = CapturedReportTimezone::capture();
        if let Some(timezone) = &timezone {
            hash.update(timezone.digest);
        }
        let query = snapshot.historical_query()?;
        let outside_timezone_range: bool = query
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_rows WHERE timestamp<?1 OR timestamp>?2)
                 OR EXISTS(SELECT 1 FROM ocq_rows WHERE timestamp<?1 OR timestamp>?2)",
                rusqlite::params![
                    jiff::Timestamp::MIN.as_millisecond(),
                    jiff::Timestamp::MAX.as_millisecond()
                ],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if outside_timezone_range {
            timezone = None;
        }
        let key = hash.finalize().into();
        let checkpoint = if timezone.is_some() {
            usage_store::ReportCheckpoint::open(query.connection(), key)?
        } else {
            usage_store::ReportCheckpoint::disabled(key)
        };
        Ok(Self {
            query,
            checkpoint,
            snapshot,
            pricing,
            aliases,
            timezone,
        })
    }

    fn fold<T>(
        &self,
        state: &mut T,
        filter: &dyn Fn(&UnifiedMessage) -> bool,
        push: &mut impl FnMut(&mut T, &UnifiedMessage),
    ) -> Result<(), String>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        self.query.admit_reader(self.snapshot)?;
        let Some(timezone) = &self.timezone else {
            let result = fold_prepared_usage_snapshot(
                self.snapshot,
                &self.query,
                self.pricing,
                LocalUsageQuery::compact(),
                &|message| filter(message),
                &mut |message| push(state, message),
                false,
            )
            .map(|_| ());
            let released = self.query.release_reader();
            return result.and(released);
        };
        let requested: HashSet<&str> = self.snapshot.clients.iter().map(String::as_str).collect();
        let passes = |message: &UnifiedMessage| {
            requested.is_empty()
                || retain_for_requested_clients(
                    &message.client,
                    &message.model_id,
                    &message.provider_id,
                    &requested,
                )
        };
        // The encoded state is consumed on restoration. Without a saved
        // accumulator this fold must start at the first selected row.
        let resume_rows = if self.checkpoint.has_restored() {
            self.checkpoint.resume_rows
        } else {
            0
        };
        if let Some(restored) = self.checkpoint.restore()? {
            *state = restored;
        } else {
            usage_store::visit_opencode_query_on(
                self.query.connection(),
                false,
                usage_store::OpenCodeSelectionMode::Report,
                false,
                &|message| {
                    message.date = timezone.date(message.timestamp);
                    apply_pricing_if_available(message, self.pricing);
                },
                &|message| passes(message) && filter(message),
                &mut |message| {
                    if passes(message) && filter(message) {
                        push(state, message);
                    }
                },
            )?;
            self.checkpoint.save(self.query.connection(), 0, state)?;
        }
        let stride = self.query.report_checkpoint_stride()?;
        let mut last = resume_rows;
        self.query.visit_report_suffix(
            last,
            &mut |position, lane, simple, guarded, mut message| {
                if lane != "grok" || self.query.grok_select(&mut message)? {
                    message.date = timezone.date(message.timestamp);
                    if simple {
                        reprice_lane_message(&mut message, self.pricing, guarded);
                    } else if !guarded || message.cost <= 0.0 {
                        apply_pricing_if_available(&mut message, self.pricing);
                    }
                    if passes(&message) {
                        if lane == "synthetic" {
                            sessions::synthetic::normalize_synthetic_gateway_fields(
                                &mut message.model_id,
                                &mut message.provider_id,
                            );
                        }
                        if filter(&message) {
                            push(state, &message);
                        }
                    }
                }
                last = position;
                if position % stride == 0 {
                    self.checkpoint
                        .save(self.query.connection(), position, state)?;
                }
                Ok(())
            },
        )?;
        if last > resume_rows && last % stride != 0 {
            self.checkpoint.save(self.query.connection(), last, state)?;
        }
        self.query.release_reader()?;
        self.checkpoint.finish(self.query.connection())
    }
}

fn visit_report_state<T>(
    state: &mut T,
    cached: Option<&HistoricalReportFold<'_>>,
    filter: &dyn Fn(&UnifiedMessage) -> bool,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
    mut push: impl FnMut(&mut T, &UnifiedMessage),
) -> Result<(), String>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    if let Some(cached) = cached {
        cached.fold(state, filter, &mut push)
    } else {
        visit(filter, &mut |message| push(state, message))
    }
}

/// Fold a complete, exact-scope Report snapshot using captured pricing only.
pub fn get_model_report_from_snapshot(
    snapshot: &LocalUsageSnapshot,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<ModelReport, String> {
    validate_report_snapshot(snapshot, &resolve_report_clients(&options))?;
    let start = Instant::now();
    let cached = HistoricalReportFold::new(snapshot, &options, pricing, "models")?;
    model_report_with_fold(&options, start, Some(&cached), |filter, sink| {
        drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
    })
}

/// Exact client/date filtering retains query-dependent OpenCode report fallback.
pub fn get_agents_report_from_snapshot(
    snapshot: &LocalUsageSnapshot,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<AgentReport, String> {
    validate_report_snapshot(snapshot, &report_scan_clients(&options))?;
    let start = Instant::now();
    let cached = HistoricalReportFold::new(snapshot, &options, pricing, "agents")?;
    agents_report_with_fold(&options, start, Some(&cached), |filter, sink| {
        drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
    })
}

/// Hourly folding uses the existing local-time and alias snapshot semantics.
pub fn get_hourly_report_from_snapshot(
    snapshot: &LocalUsageSnapshot,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<HourlyReport, String> {
    validate_report_snapshot(snapshot, &report_scan_clients(&options))?;
    let start = Instant::now();
    let cached = HistoricalReportFold::new(snapshot, &options, pricing, "hourly")?;
    hourly_report_with_fold(&options, start, Some(&cached), |filter, sink| {
        drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
    })
}

/// Graph and active-time folds read one pinned source generation, without HTTP.
pub fn generate_local_graph_from_snapshot(
    snapshot: &LocalUsageSnapshot,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
    pricing_mode: GraphPricingMode,
) -> Result<GraphResultWithContract, String> {
    validate_report_snapshot(snapshot, &resolve_report_clients(&options))?;
    let pricing = if pricing_mode == GraphPricingMode::LocalOnly {
        None
    } else {
        pricing
    };
    graph_report_with_visitor(&options, Instant::now(), pricing_mode, |filter, sink| {
        drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink)
    })
}

/// Lean snapshot graph for app consumers that drop `time_metrics` and
/// `active_time_ms` in mapping: same contributions/contract, no time fold.
pub fn lean_local_graph_from_snapshot(
    snapshot: &LocalUsageSnapshot,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
    pricing_mode: GraphPricingMode,
) -> Result<GraphResultWithContract, String> {
    validate_report_snapshot(snapshot, &resolve_report_clients(&options))?;
    let start = Instant::now();
    let pricing = if pricing_mode == GraphPricingMode::LocalOnly {
        None
    } else {
        pricing
    };
    let kind = match pricing_mode {
        GraphPricingMode::LocalOnly => "graph-local-only",
        GraphPricingMode::BestEffort => "graph-best-effort",
    };
    let cached = HistoricalReportFold::new(snapshot, &options, pricing, kind)?;
    graph_report_with_visitor_inner(
        &options,
        start,
        pricing_mode,
        true,
        Some(&cached),
        |filter, sink| drive_trusted_report_fold(snapshot, pricing, &|m| filter(m), sink),
    )
}

pub async fn get_model_report(options: ReportOptions) -> Result<ModelReport, String> {
    get_model_report_inner(None, options).await
}

pub async fn get_model_report_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
) -> Result<ModelReport, String> {
    get_model_report_inner(Some(context), options).await
}

async fn get_model_report_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: ReportOptions,
) -> Result<ModelReport, String> {
    let start = Instant::now();
    let home_dir = context
        .map(|context| context.home_dir().to_string_lossy().into_owned())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir))?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse_with_context(context).await;
    model_report_with_visitor(&options, start, |filter, sink| {
        scan_report_messages(
            context,
            &home_dir,
            &clients,
            &options,
            pricing.as_deref(),
            filter,
            sink,
        )
    })
}

fn model_report_with_visitor(
    options: &ReportOptions,
    start: Instant,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<ModelReport, String> {
    model_report_with_fold(options, start, None, visit)
}

fn model_report_with_fold(
    options: &ReportOptions,
    start: Instant,
    cached: Option<&HistoricalReportFold<'_>>,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<ModelReport, String> {
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };
    let entries =
        aggregate_model_usage_entries_fold(&options.group_by, &msg_filter, visit, cached)?;

    let (total_input, total_output, total_cache_read, total_cache_write) =
        model_report_token_totals(&entries);
    let total_messages: i32 = entries
        .iter()
        .map(|e| e.message_count)
        .fold(0_i32, i32::saturating_add);
    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

    Ok(ModelReport {
        entries,
        total_input,
        total_output,
        total_cache_read,
        total_cache_write,
        total_messages,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Default)]
struct MonthAggregator {
    models: HashSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    message_count: i32,
    cost: f64,
}

pub async fn get_monthly_report(options: ReportOptions) -> Result<MonthlyReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let pricing = load_pricing_for_local_parse().await;
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };

    let mut month_map: HashMap<String, MonthAggregator> = HashMap::new();
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = model_alias::snapshot_grouping_aliases();

    scan_messages_streaming(
        &home_dir,
        &clients,
        pricing.as_deref(),
        options.use_env_roots,
        &options.scanner_settings,
        &msg_filter,
        &mut |msg: &UnifiedMessage| {
            let month = if msg.date.len() >= 7 {
                msg.date[..7].to_string()
            } else {
                return;
            };

            let entry = month_map.entry(month).or_default();

            entry
                .models
                .insert(aliases.fold(normalize_syntactic(&msg.model_id)));
            // saturating_add so clamped (i64::MAX) buckets from a corrupt source
            // can't overflow the fold.
            entry.input = entry.input.saturating_add(msg.tokens.input);
            entry.output = entry.output.saturating_add(msg.tokens.output);
            entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
            entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
            entry.message_count = entry.message_count.saturating_add(msg.message_count.max(0));
            entry.cost += msg.cost;
        },
    )?;

    let mut entries: Vec<MonthlyUsage> = month_map
        .into_iter()
        .map(|(month, agg)| MonthlyUsage {
            month,
            models: agg.models.into_iter().collect(),
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            message_count: agg.message_count,
            cost: agg.cost,
        })
        .collect();

    entries.sort_by(|a, b| a.month.cmp(&b.month));

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

    Ok(MonthlyReport {
        entries,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct AgentAccumulator {
    clients: std::collections::BTreeSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    cost: f64,
    messages: i32,
}

impl AgentAccumulator {
    // Folds tokens like the model report's `aggregate_model_usage_entries`
    // (saturating_add — see that fn for why): the per-client parity those two
    // reports must hold depends on identical arithmetic. `message_count.max(0)`
    // matches the model/monthly aggregators (not the sessionizer's `.max(1)`).
    fn add(&mut self, msg: &UnifiedMessage) {
        self.clients.insert(msg.client.clone());
        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold.
        self.input = self.input.saturating_add(msg.tokens.input);
        self.output = self.output.saturating_add(msg.tokens.output);
        self.cache_read = self.cache_read.saturating_add(msg.tokens.cache_read);
        self.cache_write = self.cache_write.saturating_add(msg.tokens.cache_write);
        self.reasoning = self.reasoning.saturating_add(msg.tokens.reasoning);
        self.cost += msg.cost;
        self.messages = self.messages.saturating_add(msg.message_count.max(0));
    }
}

/// Agent bucket key for a message: the normalized sub-agent name, or "Main"
/// when the message carries no agent attribution. Mirrors the old FFI
/// `agents_report.rs` bucketing so the report stays byte-stable across the
/// streaming migration.
fn agent_bucket_key(msg: &UnifiedMessage) -> String {
    match msg.agent.as_deref() {
        // Copilot emits raw OTEL agent ids (e.g. "github.copilot.default",
        // "Plugin:team:slug") via #724; upstream prettifies them in its CLI TUI
        // (which we do not vendor), so apply the copilot-specific normalization
        // here on our streaming agents-report path. Every other client keeps the
        // generic normalization (opencode already normalizes at parse time).
        Some(raw) if !raw.trim().is_empty() && msg.client == "copilot" => {
            sessions::normalize_copilot_agent_name(raw)
        }
        Some(raw) if !raw.trim().is_empty() => sessions::normalize_agent_name(raw),
        _ => "Main".to_string(),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentReportEntry {
    pub agent: String,
    pub clients: Vec<String>,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
    pub cost: f64,
    pub messages: i32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentReport {
    pub entries: Vec<AgentReportEntry>,
    pub total_cost: f64,
    pub total_messages: i32,
    pub processing_time_ms: u32,
}

/// Per-sub-agent usage breakdown, ranked by cost then total tokens, with
/// unattributed messages folded into a single "Main" bucket.
///
/// Folds the SAME deduped, per-client-gated, priced message stream that the
/// model/graph/hourly/monthly reports consume (`scan_messages_streaming`), so
/// the agents report now agrees with them on copilot/codebuff/kimi/cursor/warp
/// /amp/droid/etc. totals (issue #6 — previously the agents report alone rode
/// the materialized path and skipped per-client cross-file dedup). Mirrors
/// `get_monthly_report`'s fold-in-sink shape.
pub async fn get_agents_report(options: ReportOptions) -> Result<AgentReport, String> {
    get_agents_report_inner(None, options).await
}

pub async fn get_agents_report_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
) -> Result<AgentReport, String> {
    get_agents_report_inner(Some(context), options).await
}

async fn get_agents_report_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: ReportOptions,
) -> Result<AgentReport, String> {
    let start = Instant::now();
    let home_dir = context
        .map(|context| context.home_dir().to_string_lossy().into_owned())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir))?;
    // Two-level filter: scan the producing lanes (cc-mirror variants ride the
    // claude lane), then keep only the exact requested client ids at fold time.
    let clients = report_scan_clients(&options);

    let pricing = load_pricing_for_local_parse_with_context(context).await;
    agents_report_with_visitor(&options, start, |filter, sink| {
        scan_report_messages(
            context,
            &home_dir,
            &clients,
            &options,
            pricing.as_deref(),
            filter,
            sink,
        )
    })
}

fn agents_report_with_visitor(
    options: &ReportOptions,
    start: Instant,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<AgentReport, String> {
    agents_report_with_fold(options, start, None, visit)
}

fn agents_report_with_fold(
    options: &ReportOptions,
    start: Instant,
    cached: Option<&HistoricalReportFold<'_>>,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<AgentReport, String> {
    let (_, exact) = split_report_client_filter(options);
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if !report_message_client_passes(&exact, m) {
            return false;
        }
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };

    let mut by_agent: HashMap<String, AgentAccumulator> = HashMap::new();

    let fold = |by_agent: &mut HashMap<String, AgentAccumulator>, msg: &UnifiedMessage| {
        by_agent.entry(agent_bucket_key(msg)).or_default().add(msg);
    };
    visit_report_state(&mut by_agent, cached, &msg_filter, visit, fold)?;

    let mut entries: Vec<AgentReportEntry> = by_agent
        .into_iter()
        .map(|(agent, agg)| AgentReportEntry {
            agent,
            clients: agg.clients.into_iter().collect(),
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            reasoning: agg.reasoning,
            cost: agg.cost,
            messages: agg.messages,
        })
        .collect();

    // Cost desc, then total-tokens desc — matches the old FFI agents report
    // ordering. This token-total formula MUST stay identical to the `total`
    // computed in the FFI mapper (crates/tb_core_ffi/src/agents_report.rs).
    // saturating_add so #766's i64::MAX-clamped buckets from a corrupt
    // Antigravity DB can't overflow the sort key (matches the model report's
    // saturating total; for normal >=0 tokens the result is unchanged).
    entries.sort_by(|a, b| {
        let a_total = a
            .input
            .saturating_add(a.output)
            .saturating_add(a.cache_read)
            .saturating_add(a.cache_write)
            .saturating_add(a.reasoning);
        let b_total = b
            .input
            .saturating_add(b.output)
            .saturating_add(b.cache_read)
            .saturating_add(b.cache_write)
            .saturating_add(b.reasoning);
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b_total.cmp(&a_total))
            .then_with(|| a.agent.cmp(&b.agent))
    });

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;
    let total_messages: i32 = entries
        .iter()
        .map(|e| e.messages)
        .fold(0_i32, i32::saturating_add);

    Ok(AgentReport {
        entries,
        total_cost,
        total_messages,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct HourAggregator {
    clients: HashSet<String>,
    models: HashSet<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
    message_count: i32,
    turn_count: i32,
    cost: f64,
}

/// Generate hourly usage report, keyed by "YYYY-MM-DD HH:00".
///
/// Derives the hour slot from `UnifiedMessage.timestamp` (Unix ms).
/// Falls back to date + "00:00" when timestamp is zero or missing.
pub async fn get_hourly_report(options: ReportOptions) -> Result<HourlyReport, String> {
    get_hourly_report_inner(None, options).await
}

pub async fn get_hourly_report_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
) -> Result<HourlyReport, String> {
    get_hourly_report_inner(Some(context), options).await
}

async fn get_hourly_report_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: ReportOptions,
) -> Result<HourlyReport, String> {
    let start = Instant::now();
    let home_dir = context
        .map(|context| context.home_dir().to_string_lossy().into_owned())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir))?;
    // Two-level filter: scan the producing lanes (cc-mirror variants ride the
    // claude lane), then keep only the exact requested client ids at fold time.
    let clients = report_scan_clients(&options);

    let pricing = load_pricing_for_local_parse_with_context(context).await;
    hourly_report_with_visitor(&options, start, |filter, sink| {
        scan_report_messages(
            context,
            &home_dir,
            &clients,
            &options,
            pricing.as_deref(),
            filter,
            sink,
        )
    })
}

fn hourly_report_with_visitor(
    options: &ReportOptions,
    start: Instant,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<HourlyReport, String> {
    hourly_report_with_fold(options, start, None, visit)
}

fn hourly_report_with_fold(
    options: &ReportOptions,
    start: Instant,
    cached: Option<&HistoricalReportFold<'_>>,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<HourlyReport, String> {
    use chrono::{Local, TimeZone};
    let (_, exact) = split_report_client_filter(options);
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if !report_message_client_passes(&exact, m) {
            return false;
        }
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };

    let mut hour_map: HashMap<String, HourAggregator> = HashMap::new();
    // One alias snapshot for the whole fold so a mid-fold set_model_aliases
    // cannot split messages across two grouping configs.
    let aliases = cached
        .map(|cached| std::borrow::Cow::Borrowed(&cached.aliases))
        .unwrap_or_else(|| std::borrow::Cow::Owned(model_alias::snapshot_grouping_aliases()));

    let fold = |hour_map: &mut HashMap<String, HourAggregator>, msg: &UnifiedMessage| {
        let hour_key = if let Some(timezone) = cached.and_then(|cached| cached.timezone.as_ref()) {
            timezone.hour(msg)
        } else if msg.timestamp > 0 {
            let ts_secs = msg.timestamp / 1000;
            match Local.timestamp_opt(ts_secs, 0) {
                chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:00").to_string(),
                _ => format!("{} 00:00", msg.date),
            }
        } else {
            format!("{} 00:00", msg.date)
        };

        let entry = hour_map.entry(hour_key).or_default();

        entry.clients.insert(msg.client.clone());
        entry
            .models
            .insert(aliases.fold(normalize_syntactic(&msg.model_id)));
        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold.
        entry.input = entry.input.saturating_add(msg.tokens.input);
        entry.output = entry.output.saturating_add(msg.tokens.output);
        entry.cache_read = entry.cache_read.saturating_add(msg.tokens.cache_read);
        entry.cache_write = entry.cache_write.saturating_add(msg.tokens.cache_write);
        entry.reasoning = entry.reasoning.saturating_add(msg.tokens.reasoning);
        entry.message_count = entry.message_count.saturating_add(msg.message_count.max(0));
        if msg.is_turn_start {
            entry.turn_count += 1;
        }
        entry.cost += msg.cost;
    };
    visit_report_state(&mut hour_map, cached, &msg_filter, visit, fold)?;

    let mut entries: Vec<HourlyUsage> = hour_map
        .into_iter()
        .map(|(hour, agg)| HourlyUsage {
            hour,
            clients: {
                let mut v: Vec<String> = agg.clients.into_iter().collect();
                v.sort();
                v
            },
            models: {
                let mut v: Vec<String> = agg.models.into_iter().collect();
                v.sort();
                v
            },
            input: agg.input,
            output: agg.output,
            cache_read: agg.cache_read,
            cache_write: agg.cache_write,
            message_count: agg.message_count,
            turn_count: agg.turn_count,
            reasoning: agg.reasoning,
            cost: agg.cost,
        })
        .collect();

    entries.sort_by(|a, b| a.hour.cmp(&b.hour));

    // f64's Sum identity is -0.0, so an empty report would serialize as
    // "totalCost": -0.0; adding +0.0 normalizes the sign without changing
    // any non-zero total.
    let total_cost: f64 = entries.iter().map(|e| e.cost).sum::<f64>() + 0.0;

    Ok(HourlyReport {
        entries,
        total_cost,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

/// Raw per-message rows inside `[from_ms, until_ms)`. See `WindowUsage` for
/// why this returns individual rows rather than a pre-aggregated bucket.
///
/// No `modified_after` pruning: unlike the live tail, the window here is
/// caller-chosen and can reach arbitrarily far back, so every source file has
/// to be considered.
///
/// Deliberately rejects `since`/`until`/`year`: those select a different
/// span, and silently honouring both would return a window that is not the
/// window the caller asked for. An empty or inverted `[from_ms, until_ms)` is
/// not rejected — it contains no messages, which is an answer, not a caller
/// error.
pub async fn get_window_usage(
    options: ReportOptions,
    from_ms: i64,
    until_ms: i64,
) -> Result<WindowUsage, String> {
    get_window_usage_inner(None, options, from_ms, until_ms).await
}

pub async fn get_window_usage_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
    from_ms: i64,
    until_ms: i64,
) -> Result<WindowUsage, String> {
    get_window_usage_inner(Some(context), options, from_ms, until_ms).await
}

async fn get_window_usage_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: ReportOptions,
    from_ms: i64,
    until_ms: i64,
) -> Result<WindowUsage, String> {
    let start = Instant::now();
    if options.since.is_some() || options.until.is_some() || options.year.is_some() {
        return Err("since/until/year are local-date filters and cannot be combined with an absolute window".to_string());
    }

    let home_dir = context
        .map(|context| context.home_dir().to_string_lossy().into_owned())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir))?;
    // Same two-level filter get_hourly_report_inner uses: a cc-mirror/* id
    // rides the claude lane during the scan, then the exact requested id is
    // matched per message at fold time. Correct here for the same reason it
    // is correct there — get_window_usage is a per-message report, so the
    // rows must match the exact-id grouping, not the scan's built-in
    // sweep-in-cc-mirror-variants behavior.
    let (clients, exact) = split_report_client_filter(&options);

    let pricing = load_pricing_for_local_parse_with_context(context).await;
    let msg_filter = |m: &UnifiedMessage| -> bool { report_message_client_passes(&exact, m) };

    let mut messages: Vec<WindowMessage> = Vec::new();
    let mut undated_count: u32 = 0;
    let mut fold = |msg: &UnifiedMessage| {
        if msg.timestamp <= 0 {
            undated_count = undated_count.saturating_add(1);
            return;
        }
        if msg.timestamp < from_ms || msg.timestamp >= until_ms {
            return;
        }
        messages.push(WindowMessage {
            timestamp: msg.timestamp,
            client: msg.client.clone(),
            provider_id: msg.provider_id.clone(),
            model_id: canonical_model_id(&msg.model_id),
            input: msg.tokens.input,
            output: msg.tokens.output,
            cache_read: msg.tokens.cache_read,
            cache_write: msg.tokens.cache_write,
            reasoning: msg.tokens.reasoning,
            cost: msg.cost,
            is_turn_start: msg.is_turn_start,
        });
    };
    if let Some(context) = context {
        scan_messages_streaming_with_context(
            context,
            &clients,
            pricing.as_deref(),
            None,
            &msg_filter,
            &mut fold,
        )?;
    } else {
        scan_messages_streaming(
            &home_dir,
            &clients,
            pricing.as_deref(),
            options.use_env_roots,
            &options.scanner_settings,
            &msg_filter,
            &mut fold,
        )?;
    }

    messages.sort_by_key(|m| m.timestamp);

    Ok(WindowUsage {
        messages,
        undated_count,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

fn scan_messages_streaming_with_context<F, S>(
    context: &ResolvedLocalSourceContext,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    modified_after: Option<u64>,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    scan_messages_disk(
        Some(context),
        context.home_dir(),
        clients,
        pricing,
        context.use_env_roots(),
        context.scanner_settings(),
        modified_after,
        store_query::SelectionMode::Report,
        filter,
        sink,
    )
    .map(|_| ())
}

fn scan_messages_streaming<F, S>(
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    scan_messages_disk(
        None,
        Path::new(home_dir),
        clients,
        pricing,
        use_env_roots,
        scanner_settings,
        None,
        store_query::SelectionMode::Report,
        filter,
        sink,
    )
    .map(|_| ())
}

/// Ingestion and exact historical selection spill to disk; only callers asking
/// for a materialized public result collect the selected stream.
#[allow(clippy::too_many_arguments)]
fn scan_messages_disk<F, S>(
    context: Option<&ResolvedLocalSourceContext>,
    home_dir: &Path,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
    use_env_roots: bool,
    scanner_settings: &scanner::ScannerSettings,
    modified_after: Option<u64>,
    mode: store_query::SelectionMode,
    filter: &F,
    sink: &mut S,
) -> Result<ClientCounts, String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    let captured;
    let context = if let Some(context) = context {
        context
    } else {
        captured = ResolvedLocalSourceContext::capture(
            Some(home_dir.to_path_buf()),
            use_env_roots,
            scanner_settings.clone(),
        )
        .map_err(|error| error.to_string())?;
        &captured
    };
    let (snapshot, query) = ingest_local_usage_inner(context, clients, modified_after, mode, true)?;
    let options = if mode == LocalUsageSelection::UnifiedCompatibility {
        LocalUsageQuery::default()
    } else {
        LocalUsageQuery::compact()
    };
    // Public streaming path: `buffer: true` preserves buffered semantics.
    fold_prepared_usage_snapshot(&snapshot, &query, pricing, options, filter, sink, true)
}

/// Verify discovery and source reads, then atomically publish their immutable
/// generation references. Date/year filters belong to queries, not ingestion.
pub fn synchronize_local_usage(
    context: &ResolvedLocalSourceContext,
    options: LocalParseOptions,
    selection: LocalUsageSelection,
) -> Result<LocalUsageSnapshot, String> {
    let clients = local_usage_clients(options.clients, selection);
    ingest_local_usage(context, &clients, options.modified_after, selection)
}

fn local_usage_clients(
    clients: Option<Vec<String>>,
    selection: LocalUsageSelection,
) -> Vec<String> {
    clients.unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|client| selection == LocalUsageSelection::Report || client.parse_local())
            .map(|client| client.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    })
}

/// Recollect verified changes to existing Claude, Codex and Pi-family
/// (Pi/Kimchi/Senpi/OMP) primary files while carrying the previous scope's
/// immutable source descriptors. The caller must verify captured identities
/// and parser dependencies and retain periodic authoritative discovery.
/// Unsupported plans return `None`; actual read/publication failures return
/// errors and leave the previous completed scope available.
pub fn try_synchronize_local_usage_sources(
    context: &ResolvedLocalSourceContext,
    options: LocalParseOptions,
    selection: LocalUsageSelection,
    previous: &LocalUsageSnapshot,
    changed_paths: &[PathBuf],
) -> Result<Option<LocalUsageSnapshot>, String> {
    use sha2::{Digest, Sha256};
    let _storage_management = disk_usage::ingestion_guard(context.usage_dir())?;
    if changed_paths.is_empty()
        || changed_paths.len() > 256
        || options.modified_after.is_some()
        || selection == LocalUsageSelection::UnifiedCompatibility
        || previous.root != context.usage_dir()
    {
        return Ok(None);
    }
    let mut unique = std::collections::BTreeSet::new();
    for path in changed_paths {
        if !unique.insert(path) {
            return Ok(None);
        }
    }
    let clients = local_usage_clients(options.clients, selection);
    let scope = Sha256::digest(
        bincode::serialize(&(context.identity_string(), &clients, None::<u64>, selection))
            .map_err(|error| error.to_string())?,
    );
    if previous.scope.as_slice() != scope.as_slice() {
        return Ok(None);
    }
    let query = store_query::QueryState::open(context.usage_dir())?;
    let Some(sources) = query.incremental_sources(previous, changed_paths)? else {
        return Ok(None);
    };
    // A plan covers existing regular primary files only. Missing/replaced
    // paths and parser dependency changes take the authoritative path.
    for path in changed_paths {
        if !std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .is_file()
        {
            return Ok(None);
        }
    }
    let mut snapshot =
        query.begin_scope(context.usage_dir(), context, &clients, None, selection)?;
    if !query.copy_scope_sources(previous, &snapshot)? {
        return Ok(None);
    }
    let mut cache = message_cache::SourceMessageCache::open_from_root(Some(context.usage_dir()));
    cache.ensure_open()?;
    for (path, source) in changed_paths.iter().zip(sources) {
        match source.client {
            ClientId::Claude => store_query::stage_claude(
                &query,
                &mut cache,
                path,
                context.home_dir(),
                source.index,
            )?,
            ClientId::Codex => {
                store_query::stage_codex(&query, &mut cache, path, source.index, source.headless)?
            }
            ClientId::Pi | ClientId::Kimchi | ClientId::Senpi | ClientId::Omp => {
                let identity = message_cache::CacheIdentity::for_client(source.client);
                let before = store_query::pi_source_fingerprint(&cache, identity, path)?;
                store_query::stage_pi_source(
                    &query,
                    &mut cache,
                    identity,
                    path,
                    before,
                    source.index,
                    i32::from(selection == LocalUsageSelection::Report),
                    source.tag,
                )?
            }
            _ => unreachable!("incremental plan only admits Claude, Codex, Pi-family"),
        }
    }
    cache.check()?;
    query.publish(&mut snapshot)?;
    query.record_completed_origins(&snapshot, context)?;
    Ok(Some(snapshot))
}

fn scan_usage_sources(
    context: &ResolvedLocalSourceContext,
    clients: &[String],
    modified_after: Option<u64>,
    selection: LocalUsageSelection,
) -> Result<scanner::ScanResult, String> {
    let mut scan = scanner::scan_all_clients_with_source_context(context, clients)
        .map_err(|error| error.to_string())?;
    if let Some(error) = &scan.discovery_error {
        return Err(error.clone());
    }
    if selection == LocalUsageSelection::LocalClients {
        // Scanner root deduplication spans clients. Claude needs its own
        // discovery so another client's aliased root cannot claim its files.
        let claude_files = if clients.is_empty() || clients.iter().any(|client| client == "claude")
        {
            let mut claude =
                scanner::scan_all_clients_with_source_context(context, &["claude".to_string()])
                    .map_err(|error| error.to_string())?;
            if let Some(error) = &claude.discovery_error {
                return Err(error.clone());
            }
            std::mem::take(claude.get_mut(ClientId::Claude))
        } else {
            Vec::new()
        };
        *scan.get_mut(ClientId::Claude) = claude_files;
    }
    if let Some(threshold) = modified_after {
        prune_scan_result_by_mtime(&mut scan, threshold);
    }
    Ok(scan)
}

fn validate_scope_source_generations(
    db: &rusqlite::Connection,
    scope_generation: i64,
) -> Result<(), String> {
    // Publication pins the versions verified when each source was read, not
    // an impossible simultaneous filesystem snapshot across active writers.
    message_cache::validate_scope_metadata_on(db, scope_generation)?;
    usage_store::validate_opencode_scope_sources_on(db, scope_generation)
}

fn ingest_local_usage(
    source_context: &ResolvedLocalSourceContext,
    clients: &[String],
    modified_after: Option<u64>,
    mode: LocalUsageSelection,
) -> Result<LocalUsageSnapshot, String> {
    ingest_local_usage_inner(source_context, clients, modified_after, mode, false)
        .map(|(snapshot, _)| snapshot)
}

fn ingest_local_usage_inner(
    source_context: &ResolvedLocalSourceContext,
    clients: &[String],
    modified_after: Option<u64>,
    mode: LocalUsageSelection,
    prepare: bool,
) -> Result<(LocalUsageSnapshot, store_query::QueryState), String> {
    use message_cache::{CacheIdentity, SourceFingerprint};
    use store_query::{ingest_sources, stage_claude, stage_codex, QueryState, SelectionMode};
    let home_dir = source_context.home_dir();
    let _storage_management = disk_usage::ingestion_guard(source_context.usage_dir())?;
    let local_clients = mode == SelectionMode::LocalClients;
    let scan_result = scan_usage_sources(source_context, clients, modified_after, mode)?;
    let headless_roots = scanner::headless_roots_with_source_context(source_context)
        .map_err(|error| error.to_string())?;
    // Durable usage facts live in the app-owned usage root, never the cache.
    // The legacy cache dir remains an ingestion input only via migration.
    let root = source_context.usage_dir().to_path_buf();
    usage_migration::prepare(&root, source_context.legacy_usage_roots())?;
    let mut cache = message_cache::SourceMessageCache::open_from_root(Some(&root));
    cache.ensure_open()?;
    let query = QueryState::open(&root)?;
    let mut snapshot = query.begin_scope(&root, source_context, clients, modified_after, mode)?;
    query.seed_cache_descriptors()?;
    let include_all = clients.is_empty();
    let include_synthetic = include_all || clients.iter().any(|client| client == "synthetic");
    usage_store::synchronize_opencode_sources(
        &scan_result.opencode_dbs,
        scan_result.get(ClientId::OpenCode),
        &root,
        &mut |db, path, sqlite, index, generation| {
            query.register_opencode(db, path, sqlite, index, generation)
        },
    )?;
    for (index, path) in scan_result.get(ClientId::Claude).iter().enumerate() {
        stage_claude(&query, &mut cache, path, home_dir, index)?;
    }
    for (index, path) in scan_result.get(ClientId::Codex).iter().enumerate() {
        stage_codex(
            &query,
            &mut cache,
            path,
            index,
            is_headless_path(path, &headless_roots),
        )?;
    }
    // Source-local native vectors remain only for unchanged third-party parser
    // APIs; never retain multiple misses or historical query state in memory.
    macro_rules! ingest {
        ($client:expr, $parse:expr) => {
            ingest!($client, $parse, SourceFingerprint::from_path_samples_only);
        };
        ($client:expr, $parse:expr, $fingerprint:expr) => {
            ingest!(
                $client,
                $parse,
                $fingerprint,
                scan_result.get($client),
                false,
                false,
                false
            );
        };
        ($client:expr, $parse:expr, $fingerprint:expr, $paths:expr, $composite:expr, $empty:expr, $remove:expr) => {{
            let client = $client;
            let paths = $paths;
            if !local_clients || client != ClientId::Cursor {
                ingest_sources(
                    &query,
                    &mut cache,
                    CacheIdentity::for_client(client),
                    paths,
                    client.as_str(),
                    if $composite {
                        SelectionMode::LocalClients
                    } else {
                        mode
                    },
                    $fingerprint,
                    $parse,
                    |path| {
                        if client == ClientId::Kiro {
                            sessions::kiro::is_kiro_global_storage_source(path)
                        } else {
                            store_query::local_dedups(client, path)
                        }
                    },
                    $empty,
                    $remove,
                )?;
            }
        }};
    }
    macro_rules! ingest_streamed {
        ($client:expr, $visit:expr) => {
            ingest_streamed!($client, $visit, SourceFingerprint::from_path_samples_only);
        };
        ($client:expr, $visit:expr, $fingerprint:expr) => {
            ingest_streamed!($client, $visit, $fingerprint, scan_result.get($client));
        };
        ($client:expr, $visit:expr, $fingerprint:expr, $paths:expr) => {
            ingest_streamed!($client, $visit, $fingerprint, $paths, false, false);
        };
        ($client:expr, $visit:expr, $fingerprint:expr, $paths:expr, $composite:expr, $empty:expr) => {{
            let client = $client;
            if !local_clients || client != ClientId::Cursor {
                store_query::ingest_sources_streamed(
                    &query,
                    &mut cache,
                    CacheIdentity::for_client(client),
                    $paths,
                    client.as_str(),
                    if $composite {
                        SelectionMode::LocalClients
                    } else {
                        mode
                    },
                    $fingerprint,
                    $visit,
                    |path| store_query::local_dedups(client, path),
                    $empty,
                )?;
            }
        }};
    }
    ingest!(
        ClientId::Copilot,
        sessions::copilot::parse_copilot_file,
        SourceFingerprint::from_path_samples_only,
        scan_result.get(ClientId::Copilot),
        true,
        false,
        false
    );
    if let Some(path) = scan_result.copilot_desktop_db.as_deref() {
        let identity = CacheIdentity::for_client(ClientId::Copilot);
        let index = scan_result.get(ClientId::Copilot).len();
        let before = SourceFingerprint::from_copilot_desktop_path(path);
        let hit = store_query::cache_hit(&cache, identity, path, before.as_ref(), false)?;
        if let Some(metadata) = hit {
            store_query::stage_cached(
                &query, &cache, identity, path, &metadata, "copilot", index, 0, true, false,
            )?;
        } else {
            // Special source index/tag are part of the lane contract: index
            // `len(copilot files)` with tag=true, staged unconditionally like
            // before (no LocalClients gate on this lane).
            std::fs::File::open(path).map_err(|error| error.to_string())?;
            store_query::stage_streamed_source(
                &query,
                &mut cache,
                identity,
                path,
                before,
                SourceFingerprint::from_copilot_desktop_path,
                |sink| sessions::copilot_desktop::visit_copilot_desktop_db(path, sink),
                "copilot",
                index,
                0,
                true,
            )?;
        }
    }
    ingest_streamed!(ClientId::Cursor, sessions::cursor::visit_cursor_file);
    ingest!(ClientId::Warp, sessions::warp::parse_warp_file);
    ingest!(ClientId::Amp, sessions::amp::parse_amp_file);
    ingest_streamed!(ClientId::Codebuff, sessions::codebuff::visit_codebuff_file);
    ingest!(
        ClientId::Droid,
        sessions::droid::parse_droid_file,
        SourceFingerprint::from_droid_path
    );
    ingest_streamed!(
        ClientId::OpenClaw,
        sessions::openclaw::visit_openclaw_transcript
    );
    for client in [ClientId::Pi, ClientId::Kimchi, ClientId::Senpi, ClientId::Omp] {
        store_query::ingest_pi_sources(
            &query, &mut cache, CacheIdentity::for_client(client), scan_result.get(client), mode,
        )?;
    }
    ingest!(
        ClientId::Kimi,
        parse_kimi_source,
        SourceFingerprint::from_kimi_path
    );
    ingest!(ClientId::Junie, sessions::junie::parse_junie_file);
    ingest!(
        ClientId::OpenCodeReview,
        sessions::opencodereview::parse_opencodereview_file
    );
    ingest!(ClientId::Augment, sessions::augment::parse_augment_file);
    ingest!(
        ClientId::Hindsight,
        sessions::hindsight::parse_hindsight_file
    );
    ingest!(ClientId::Muse, sessions::muse::parse_muse_file);
    ingest!(ClientId::Reasonix, sessions::reasonix::parse_reasonix_file);
    ingest!(ClientId::Qwen, sessions::qwen::parse_qwen_file);
    ingest_streamed!(
        ClientId::RooCode,
        sessions::roocode::visit_roocode_file,
        SourceFingerprint::from_roo_path
    );
    ingest_streamed!(
        ClientId::KiloCode,
        sessions::kilocode::visit_kilocode_file,
        SourceFingerprint::from_roo_path
    );
    ingest_streamed!(
        ClientId::Cline,
        sessions::cline::visit_cline_file,
        SourceFingerprint::from_roo_path
    );
    ingest!(
        ClientId::Jcode,
        sessions::jcode::parse_jcode_file,
        SourceFingerprint::from_jcode_path
    );
    ingest!(
        ClientId::Grok,
        sessions::grok::parse_grok_file,
        SourceFingerprint::from_grok_path,
        scan_result.get(ClientId::Grok),
        true,
        false,
        false
    );
    ingest_streamed!(
        ClientId::MiMoCode,
        sessions::micode::visit_micode_sqlite,
        SourceFingerprint::from_sqlite_path
    );
    ingest!(ClientId::Mux, sessions::mux::parse_mux_file);
    ingest_streamed!(
        ClientId::Zcode,
        sessions::zcode::visit_zcode_sqlite,
        SourceFingerprint::from_sqlite_path,
        scan_result.zcode_db.as_slice()
    );
    ingest!(
        ClientId::Kiro,
        sessions::kiro::parse_kiro_file,
        SourceFingerprint::from_kiro_path,
        scan_result.get(ClientId::Kiro),
        true,
        false,
        mode == SelectionMode::Report
    );
    // Gemini's parser marks transient/incomplete sources as uncacheable.
    let gemini_identity = CacheIdentity::for_client(ClientId::Gemini);
    for (index, path) in scan_result.get(ClientId::Gemini).iter().enumerate() {
        let fingerprint = SourceFingerprint::from_path_samples_only(path);
        let hit =
            store_query::cache_hit(&cache, gemini_identity, path, fingerprint.as_ref(), false)?;
        query.classify("gemini", index, hit.is_none())?;
        if let Some(metadata) = hit {
            store_query::stage_cached(
                &query,
                &cache,
                gemini_identity,
                path,
                &metadata,
                "gemini",
                index,
                0,
                false,
                false,
            )?;
        }
    }
    for (index, path) in scan_result.get(ClientId::Gemini).iter().enumerate() {
        if !query.is_miss("gemini", index)? {
            continue;
        }
        let fingerprint = SourceFingerprint::from_path_samples_only(path);
        std::fs::File::open(path).map_err(|error| error.to_string())?;
        let parsed = sessions::utils::source_read_scope(path, || {
            sessions::gemini::parse_gemini_file_with_cache_status(path)
        })?;
        if SourceFingerprint::from_path_samples_only(path) != fingerprint {
            return Err(
                "Gemini source changed during ingestion; previous generation retained".into(),
            );
        }
        if !parsed.cacheable && mode != SelectionMode::UnifiedCompatibility {
            return Err("Gemini source parse incomplete; previous scope retained".into());
        }
        let fingerprint =
            fingerprint.ok_or("Gemini source fingerprint unavailable; previous scope retained")?;
        let phase = if mode == SelectionMode::Report { 1 } else { 0 };
        let metadata = cache.write_source(
            gemini_identity,
            path,
            fingerprint,
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                query.register_cache_on(db, path, metadata, "gemini", index, phase, false, false)
            },
            |writer| {
                writer.set_cacheable(parsed.cacheable)?;
                for message in &parsed.messages {
                    writer.push(message, false, false)?;
                }
                Ok(())
            },
        )?;
        store_query::stage_cached(
            &query,
            &cache,
            gemini_identity,
            path,
            &metadata,
            "gemini",
            index,
            if mode == SelectionMode::Report { 1 } else { 0 },
            false,
            false,
        )?;
    }
    // Dedicated SQLite roots also use the same source cache and bounded row replay.
    ingest!(
        ClientId::Kilo,
        sessions::kilo::parse_kilo_sqlite,
        SourceFingerprint::from_sqlite_path,
        scan_result.kilo_db.as_slice(),
        false,
        true,
        false
    );
    let hermes_paths = scan_result.hermes_db_paths();
    ingest_streamed!(
        ClientId::Hermes,
        sessions::hermes::visit_hermes_sqlite,
        SourceFingerprint::from_sqlite_path,
        &hermes_paths,
        true,
        true
    );
    ingest!(
        ClientId::AntigravityCli,
        sessions::antigravity_cli::parse_antigravity_cli_file,
        SourceFingerprint::from_sqlite_path
    );
    ingest!(ClientId::Gjc, sessions::gjc::parse_gjc_file);
    ingest_streamed!(
        ClientId::Goose,
        sessions::goose::visit_goose_sqlite,
        SourceFingerprint::from_sqlite_path,
        scan_result.goose_db.as_slice(),
        false,
        true
    );
    let zed_paths = scan_result.zed_db_paths();
    ingest!(
        ClientId::Zed,
        sessions::zed::parse_zed_sqlite,
        SourceFingerprint::from_sqlite_path,
        &zed_paths,
        false,
        true,
        false
    );
    ingest_sources(
        &query,
        &mut cache,
        CacheIdentity::for_client(ClientId::Kiro),
        scan_result.kiro_db.as_slice(),
        "kiro-db",
        mode,
        SourceFingerprint::from_sqlite_path,
        sessions::kiro::parse_kiro_sqlite,
        |_| false,
        true,
        false,
    )?;

    for (index, source) in scan_result.crush_dbs.iter().enumerate() {
        let fingerprint = SourceFingerprint::from_sqlite_path(&source.db_path);
        let identity = CacheIdentity::for_client(ClientId::Crush);
        if let Some(metadata) = store_query::cache_hit(
            &cache,
            identity,
            &source.db_path,
            fingerprint.as_ref(),
            true,
        )? {
            query.register_cache(&source.db_path, &metadata, "crush", index, 0, false, false)?;
        } else {
            std::fs::File::open(&source.db_path).map_err(|error| error.to_string())?;
            let messages = sessions::utils::source_read_scope(&source.db_path, || {
                sessions::crush::parse_crush_sqlite(&source.db_path)
            })?;
            if SourceFingerprint::from_sqlite_path(&source.db_path) != fingerprint {
                return Err(
                    "Crush source changed during ingestion; previous generation retained".into(),
                );
            }
            store_query::stage_source(
                &query,
                &mut cache,
                identity,
                &source.db_path,
                fingerprint,
                messages,
                "crush",
                index,
                0,
                false,
                false,
            )?;
        }
        query.set_workspace_reference(
            "crush",
            index,
            &source.workspace_key,
            &source.workspace_label,
        )?;
    }
    ingest_streamed!(
        ClientId::Antigravity,
        sessions::antigravity::visit_antigravity_file
    );
    ingest_streamed!(
        ClientId::Trae,
        |path, sink| sessions::trae::visit_trae_file("trae", path, sink)
    );
    if let Some(path) = scan_result
        .synthetic_db
        .as_ref()
        .filter(|_| include_synthetic)
    {
        ingest_sources(
            &query,
            &mut cache,
            CacheIdentity::synthetic(),
            std::slice::from_ref(path),
            "synthetic",
            mode,
            SourceFingerprint::from_sqlite_path,
            sessions::synthetic::parse_octofriend_sqlite,
            |_| false,
            true,
            false,
        )?;
    }
    // Durable-known identities the scanner no longer discovers keep their
    // canonical generations referenced by the new scope — AFTER every live
    // source stage, so archive orders allocate as per-lane SQL max+1 and can
    // never collide with live orders or evict a live cohort. Account/root /
    // client filtering happens in fold queries, not by dropping identities.
    usage_store::carry_absent_opencode_sources(
        &scan_result,
        &scan_result.opencode_dbs,
        scan_result.get(ClientId::OpenCode),
        &root,
        snapshot.generation,
        &mut |db, path, sqlite, index, generation| {
            query.register_opencode(db, path, sqlite, index, generation)
        },
    )?;
    store_query::carry_absent_durable_sources(
        &query,
        &cache,
        source_context,
        &scan_result,
        home_dir,
        clients,
    )?;
    cache.check()?;
    // Every source was admitted against its own read boundary. Later writes
    // remain dirty in the consumer's pre-ingestion watch manifest and belong
    // to the next generation; they cannot invalidate already captured facts.
    if prepare {
        query.publish_prepared(&mut snapshot)?;
    } else {
        query.publish(&mut snapshot)?;
    }
    // Record only on success: staged rows of this completed scope become
    // same-context origin claims for vanished dynamic folders.
    query.record_completed_origins(&snapshot, source_context)?;
    Ok((snapshot, query))
}

/// Read a pinned store generation without rediscovery, source parsing, or
/// pricing requests. Range gates follow exact identity/authority selection.
pub fn visit_local_usage_snapshot<F, S>(
    snapshot: &LocalUsageSnapshot,
    query: LocalUsageQuery,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    visit_priced_local_usage_snapshot(snapshot, None, query, filter, sink)
}

/// Visit a store snapshot using a caller-pinned catalog and the engine's
/// existing pricing, provider authority, and multiplier rules.
pub fn visit_priced_local_usage_snapshot<F, S>(
    snapshot: &LocalUsageSnapshot,
    pricing: Option<&pricing::PricingService>,
    query: LocalUsageQuery,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    fold_usage_snapshot(snapshot, pricing, query, filter, sink).map(|_| ())
}

/// Capture the same optional pricing catalog used by local report consumers.
pub async fn capture_local_usage_pricing(
    context: &ResolvedLocalSourceContext,
) -> Option<Arc<pricing::PricingService>> {
    load_pricing_for_local_parse_with_context(Some(context)).await
}

pub fn local_usage_model_catalog(snapshot: &LocalUsageSnapshot) -> Result<Vec<String>, String> {
    let mut models = std::collections::BTreeSet::new();
    visit_local_usage_snapshot(
        snapshot,
        LocalUsageQuery::compact(),
        &|_| true,
        &mut |message| {
            models.insert(canonical_model_id(&message.model_id));
        },
    )?;
    Ok(models.into_iter().collect())
}

fn fold_usage_snapshot<F, S>(
    snapshot: &LocalUsageSnapshot,
    pricing: Option<&pricing::PricingService>,
    options: LocalUsageQuery,
    filter: &F,
    sink: &mut S,
) -> Result<ClientCounts, String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    let query = snapshot.recent_query()?;
    let prepared = query.prepare_query(snapshot, options);
    let released = query.unpin();
    prepared.and(released)?;
    // `buffer: true` keeps the serialized `sq_output` flush so public
    // `visit_*` retains buffered failure/chronological/reentrant semantics.
    let result =
        fold_prepared_usage_snapshot(snapshot, &query, pricing, options, filter, sink, true);
    query.finish_query_output();
    result
}
/// Shared trusted-fold driver: one arbitration, no serialize roundtrip.
///
/// Public `visit_*` keeps the buffered/chronological/reentrant contract; the
/// four snapshot reports (model/agents/hourly/graph) fold the same selected
/// stream directly. Partial fold state lives in the caller and is dropped on
/// any `Err`, so a failed fold never publishes. FP order follows first-seen
/// traversal, identical to the buffered `sq_output` rank order it replaces.
fn drive_trusted_report_fold<F>(
    snapshot: &LocalUsageSnapshot,
    pricing: Option<&pricing::PricingService>,
    filter: &F,
    sink: &mut dyn FnMut(&UnifiedMessage),
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
{
    // `buffer: false` folds the same selected stream straight into the caller's
    // sink: no `sq_output` serialize roundtrip, no full-history Vec, and the
    // error path returns before any partial aggregate can publish.
    fold_usage_snapshot_direct(snapshot, pricing, &|m| filter(m), sink).map(|_| ())
}

fn fold_usage_snapshot_direct<F>(
    snapshot: &LocalUsageSnapshot,
    pricing: Option<&pricing::PricingService>,
    filter: &F,
    sink: &mut dyn FnMut(&UnifiedMessage),
) -> Result<ClientCounts, String>
where
    F: Fn(&UnifiedMessage) -> bool,
{
    let query = snapshot.recent_query()?;
    let options = LocalUsageQuery::compact();
    let folded = query
        .prepare_trusted_query(snapshot, options)
        .and_then(|_| {
            fold_prepared_usage_snapshot(snapshot, &query, pricing, options, filter, sink, false)
        });
    query.finish_query_output();
    let released = query.unpin();
    folded.and_then(|counts| released.map(|_| counts))
}

fn fold_prepared_usage_snapshot<F, S>(
    snapshot: &LocalUsageSnapshot,
    query: &store_query::QueryState,
    pricing: Option<&pricing::PricingService>,
    options: LocalUsageQuery,
    filter: &F,
    sink: &mut S,
    buffer: bool,
) -> Result<ClientCounts, String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage) + ?Sized,
{
    use store_query::SelectionMode;
    if options
        .from_ms
        .zip(options.until_ms)
        .is_some_and(|(from, until)| from > until)
    {
        return Err("usage query range is reversed".into());
    }
    (|| {
        // Marker lifetime is owned by preparation (`prepare_query` resets at
        // entry; `take_report_reused` consumes at dispatch). The fold keeps
        // eligibility-before-selection and never clears here.
        query.set_client_eligibility(&snapshot.clients)?;
        let mode = snapshot.mode;
        let include_all = snapshot.clients.is_empty();
        let include_synthetic =
            include_all || snapshot.clients.iter().any(|client| client == "synthetic");
        let requested: HashSet<&str> = snapshot.clients.iter().map(String::as_str).collect();
        let passes_client = |message: &UnifiedMessage| {
            include_all
                || retain_for_requested_clients(
                    &message.client,
                    &message.model_id,
                    &message.provider_id,
                    &requested,
                )
        };
        let mut counts = ClientCounts::new();
        let count_message =
            |counts: &mut ClientCounts, client: ClientId, message: &UnifiedMessage| {
                let count = if matches!(
                    client,
                    ClientId::OpenCode
                        | ClientId::Claude
                        | ClientId::Codex
                        | ClientId::Copilot
                        | ClientId::Gemini
                        | ClientId::Amp
                        | ClientId::Codebuff
                        | ClientId::Droid
                        | ClientId::OpenClaw
                        | ClientId::Pi
                        | ClientId::Qwen
                        | ClientId::RooCode
                        | ClientId::Antigravity
                        | ClientId::Trae
                ) {
                    1
                } else {
                    message.message_count.max(0)
                };
                counts.set(client, counts.get(client).saturating_add(count));
            };
        let selection_ignores_filter =
            mode != SelectionMode::Report || snapshot.selection_ignores_filter;
        let opencode_mode = if selection_ignores_filter {
            usage_store::OpenCodeSelectionMode::Materialized
        } else {
            usage_store::OpenCodeSelectionMode::Report
        };
        let mut output_error = None;
        let opencode_eligible = std::cell::Cell::new(false);
        usage_store::visit_opencode_query_on(
            query.connection(),
            query.is_indexed(),
            opencode_mode,
            options.hydrate_opencode_aliases,
            &|message| {
                message.refresh_derived_fields();
                apply_pricing_if_available(message, pricing);
                opencode_eligible
                    .set(passes_client(message) && options.contains(message.timestamp));
                if mode != SelectionMode::Report && include_synthetic {
                    sessions::synthetic::normalize_synthetic_gateway_fields(
                        &mut message.model_id,
                        &mut message.provider_id,
                    );
                }
            },
            &|message| {
                if selection_ignores_filter
                    && options.from_ms.is_none()
                    && options.until_ms.is_none()
                {
                    return true;
                }
                if !opencode_eligible.get() {
                    return false;
                }
                filter(message)
            },
            &mut |message| {
                if output_error.is_some() {
                    return;
                }
                count_message(&mut counts, ClientId::OpenCode, message);
                if !opencode_eligible.get() {
                    return;
                }
                if filter(message) {
                    if buffer {
                        if let Err(error) = query.buffer_output(message) {
                            output_error = Some(error);
                        }
                    } else {
                        sink(message);
                    }
                }
            },
        )?;
        if let Some(error) = output_error {
            return Err(error);
        }
        // Exact-generation Report reuse: `prepare_query` SQL-copied this
        // generation's own indexed winners, so the fold skips only their
        // repeated arbitration. Every other lane selects fresh here.
        let reused = query.take_report_reused();
        let mut emit_lane = |lane: &str,
                             client: Option<ClientId>,
                             simple: bool,
                             guarded: bool|
         -> Result<(), String> {
            if !reused || !matches!(lane, "claude" | "codex") {
                query.select_lane(lane, mode, simple)?;
            }
            query.visit_selected_lane(lane, options, &mut |mut message, _, _| {
                if lane == "grok" && !query.is_indexed() && !query.grok_select(&mut message)? {
                    return Ok(());
                }
                message.refresh_derived_fields();
                if simple && mode == SelectionMode::Report {
                    reprice_lane_message(&mut message, pricing, guarded);
                } else if !guarded || message.cost <= 0.0 {
                    apply_pricing_if_available(&mut message, pricing);
                }
                if let Some(client) = client {
                    count_message(&mut counts, client, &message);
                }
                if !passes_client(&message) {
                    return Ok(());
                }
                if mode != SelectionMode::Report && include_synthetic
                    || lane == "synthetic" && mode == SelectionMode::Report
                {
                    sessions::synthetic::normalize_synthetic_gateway_fields(
                        &mut message.model_id,
                        &mut message.provider_id,
                    );
                }
                if filter(&message) {
                    if buffer {
                        query.buffer_output(&message)?;
                    } else {
                        sink(&message);
                    }
                }
                Ok(())
            })
        };
        for_each_usage_lane(mode, &mut emit_lane)?;
        if buffer {
            query.visit_output(options, &mut |message| sink(message))?;
        }
        Ok(counts)
    })()
}

pub(crate) fn for_each_usage_lane(
    mode: store_query::SelectionMode,
    emit_lane: &mut impl FnMut(&str, Option<ClientId>, bool, bool) -> Result<(), String>,
) -> Result<(), String> {
    use store_query::SelectionMode;
    emit_lane("claude", Some(ClientId::Claude), false, false)?;
    emit_lane("codex", Some(ClientId::Codex), false, false)?;
    emit_lane("copilot", Some(ClientId::Copilot), true, false)?;
    if mode != SelectionMode::Report {
        emit_lane("gemini", Some(ClientId::Gemini), false, false)?;
    }
    for client in [
        ClientId::Cursor,
        ClientId::Warp,
        ClientId::Amp,
        ClientId::Codebuff,
        ClientId::Droid,
        ClientId::OpenClaw,
        ClientId::Pi,
        ClientId::Kimchi,
        ClientId::Senpi,
        ClientId::Omp,
        ClientId::Kimi,
        ClientId::Junie,
        ClientId::OpenCodeReview,
        ClientId::Augment,
        ClientId::Hindsight,
        ClientId::Muse,
        ClientId::Reasonix,
        ClientId::Qwen,
        ClientId::RooCode,
        ClientId::KiloCode,
        ClientId::Cline,
        ClientId::Jcode,
    ] {
        if mode == SelectionMode::LocalClients && client == ClientId::Warp {
            continue;
        }
        emit_lane(client.as_str(), Some(client), true, false)?;
    }
    if mode == SelectionMode::Report {
        for client in [
            ClientId::Grok,
            ClientId::MiMoCode,
            ClientId::Mux,
            ClientId::Zcode,
        ] {
            emit_lane(
                client.as_str(),
                Some(client),
                true,
                client == ClientId::MiMoCode,
            )?;
        }
        emit_lane("kiro", Some(ClientId::Kiro), false, false)?;
        emit_lane("gemini", Some(ClientId::Gemini), false, false)?;
        for client in [
            ClientId::Kilo,
            ClientId::Hermes,
            ClientId::AntigravityCli,
            ClientId::Gjc,
            ClientId::Goose,
            ClientId::Zed,
        ] {
            emit_lane(
                client.as_str(),
                Some(client),
                false,
                client == ClientId::Hermes,
            )?;
        }
        emit_lane("kiro-db", Some(ClientId::Kiro), false, false)?;
        emit_lane("crush", Some(ClientId::Crush), false, false)?;
        emit_lane("antigravity", Some(ClientId::Antigravity), false, false)?;
        emit_lane("synthetic", None, false, false)?;
        emit_lane("trae", Some(ClientId::Trae), false, false)?;
    } else {
        emit_lane("micode", Some(ClientId::MiMoCode), true, true)?;
        emit_lane("gjc", Some(ClientId::Gjc), false, false)?;
        emit_lane("grok", Some(ClientId::Grok), true, false)?;
        emit_lane("mux", Some(ClientId::Mux), true, false)?;
        for client in [ClientId::Kilo, ClientId::Hermes, ClientId::Goose] {
            emit_lane(
                client.as_str(),
                Some(client),
                false,
                client == ClientId::Hermes,
            )?;
        }
        if mode == SelectionMode::LocalClients {
            emit_lane("zcode", Some(ClientId::Zcode), true, false)?;
            emit_lane("zed", Some(ClientId::Zed), false, false)?;
        } else {
            emit_lane("zed", Some(ClientId::Zed), false, false)?;
            emit_lane("zcode", Some(ClientId::Zcode), true, false)?;
        }
        emit_lane("kiro", Some(ClientId::Kiro), false, false)?;
        emit_lane("kiro-db", Some(ClientId::Kiro), false, false)?;
        emit_lane("crush", Some(ClientId::Crush), false, false)?;
        emit_lane("antigravity", Some(ClientId::Antigravity), false, false)?;
        emit_lane(
            "antigravity-cli",
            Some(ClientId::AntigravityCli),
            false,
            false,
        )?;
        emit_lane("trae", Some(ClientId::Trae), false, false)?;
        if mode == SelectionMode::LocalClients {
            emit_lane("warp", Some(ClientId::Warp), true, false)?;
        }
        emit_lane("synthetic", None, false, false)?;
    }
    Ok(())
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CostCoverageFold {
    known: bool,
    unknown: bool,
}

impl CostCoverageFold {
    fn observe(&mut self, message: &UnifiedMessage) {
        // Structural rows (no usage and no cost authority) do not affect
        // coverage. ProviderReported remains relevant even at a legitimate
        // zero cost; CostSource is the provenance source of truth.
        let cost_relevant = message.tokens.total() > 0
            || message.message_count > 0
            || message.cost > 0.0
            || message.cost_source == CostSource::ProviderReported;
        if !cost_relevant {
            return;
        }

        match message.cost_source {
            CostSource::ProviderReported | CostSource::Estimated => self.known = true,
            CostSource::PartiallyEstimated => {
                self.known = true;
                self.unknown = true;
            }
            CostSource::Unknown => self.unknown = true,
        }
    }

    fn finish(self) -> CostCoverage {
        if !self.unknown {
            CostCoverage::Complete
        } else if self.known {
            CostCoverage::Partial
        } else {
            CostCoverage::None
        }
    }
}

fn coverage_for_messages<'a>(
    messages: impl IntoIterator<Item = &'a UnifiedMessage>,
) -> CostCoverage {
    let mut fold = CostCoverageFold::default();
    for message in messages {
        fold.observe(message);
    }
    fold.finish()
}

async fn resolve_graph_pricing<F, Fut>(
    mode: GraphPricingMode,
    loader: F,
) -> Result<Option<Arc<pricing::PricingService>>, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Arc<pricing::PricingService>>, String>>,
{
    match mode {
        // The branch is deliberately before invoking the closure: local-first
        // has no reachable pricing loader or outbound pricing seam.
        GraphPricingMode::LocalOnly => Ok(None),
        GraphPricingMode::BestEffort => loader().await,
    }
}

async fn generate_graph_with_loaded_pricing(
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
    pricing_mode: GraphPricingMode,
) -> Result<GraphResultWithContract, String> {
    generate_graph_with_loaded_pricing_inner(None, options, pricing, pricing_mode, false).await
}

async fn generate_graph_with_loaded_pricing_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: ReportOptions,
    pricing: Option<&pricing::PricingService>,
    pricing_mode: GraphPricingMode,
    lean: bool,
) -> Result<GraphResultWithContract, String> {
    let start = Instant::now();
    let home_dir = context
        .map(|context| context.home_dir().to_string_lossy().into_owned())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir))?;
    let clients = resolve_report_clients(&options);
    graph_report_with_visitor_inner(&options, start, pricing_mode, lean, None, |filter, sink| {
        scan_report_messages(
            context, &home_dir, &clients, &options, pricing, filter, sink,
        )
    })
}

fn graph_report_with_visitor(
    options: &ReportOptions,
    start: Instant,
    pricing_mode: GraphPricingMode,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<GraphResultWithContract, String> {
    graph_report_with_visitor_inner(options, start, pricing_mode, false, None, visit)
}

#[cfg(test)]
/// Lean app graph path: identical wire payload shape with `time_metrics: None`
/// and unset `active_time_ms`; skips the sessionize fold entirely. Only for
/// callers whose mapping drops those fields anyway.
pub(crate) fn lean_graph_report_with_visitor(
    options: &ReportOptions,
    start: Instant,
    pricing_mode: GraphPricingMode,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<GraphResultWithContract, String> {
    graph_report_with_visitor_inner(options, start, pricing_mode, true, None, visit)
}

fn graph_report_with_visitor_inner(
    options: &ReportOptions,
    start: Instant,
    pricing_mode: GraphPricingMode,
    lean: bool,
    cached: Option<&HistoricalReportFold<'_>>,
    visit: impl FnOnce(
        &dyn Fn(&UnifiedMessage) -> bool,
        &mut dyn FnMut(&UnifiedMessage),
    ) -> Result<(), String>,
) -> Result<GraphResultWithContract, String> {
    // Build filter closure from report options (year/since/until).
    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };

    // Dual-sink: day aggregator + sessionize accumulator fed in one pass.
    // scan_messages_streaming handles all dedup (trae keep-latest + dedup_key gate).
    // StreamingAggregator.feed_pre_deduped() bypasses its internal dedup gate
    // since the driver already guarantees uniqueness.
    let mut state = (
        aggregator::StreamingAggregator::new(),
        CostCoverageFold::default(),
    );
    let mut sess_agg = (!lean).then(store_query::DiskTimeFold::new).transpose()?;
    let fold = |state: &mut (aggregator::StreamingAggregator, CostCoverageFold),
                message: &UnifiedMessage| {
        state.1.observe(message);
        state.0.feed_pre_deduped(message);
        if let Some(sess_agg) = sess_agg.as_mut() {
            sess_agg.feed(message);
        }
    };
    visit_report_state(&mut state, cached, &msg_filter, visit, fold)?;
    let (day_agg, coverage) = state;

    let contract = GraphMetaContract {
        pricing_mode,
        cost_coverage: coverage.finish(),
    };

    let contributions = day_agg.finalize();

    // Finalize the optional time fold BEFORE capturing elapsed: the full-graph
    // contract bills sessionize work inside `meta.processing_time_ms`, exactly
    // like the pre-lean baseline. The lean path skips the fold entirely.
    let timed = if let Some(sess_agg) = sess_agg {
        Some(sess_agg.finish(sessionize::DEFAULT_IDLE_GAP_MS)?)
    } else {
        None
    };

    let processing_time_ms = start.elapsed().as_millis() as u32;
    let mut result = aggregator::generate_graph_result(contributions, processing_time_ms);
    if let Some((time_metrics, daily_active_time)) = timed {
        result.time_metrics = Some(time_metrics);
        for contribution in &mut result.contributions {
            if let Some(&ms) = daily_active_time.get(&contribution.date) {
                contribution.active_time_ms = Some(ms);
            }
        }
    }

    Ok(GraphResultWithContract {
        graph: result,
        contract,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeMetricsReport {
    pub metrics: sessionize::TimeMetrics,
    pub processing_time_ms: u32,
}

pub async fn get_time_metrics_report(options: ReportOptions) -> Result<TimeMetricsReport, String> {
    let start = Instant::now();

    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = resolve_report_clients(&options);

    let year_prefix = options.year.as_ref().map(|y| format!("{}-", y));
    let since_s = options.since.clone();
    let until_s = options.until.clone();
    let msg_filter = |m: &UnifiedMessage| -> bool {
        if let Some(ref yp) = year_prefix {
            if !m.date.starts_with(yp.as_str()) {
                return false;
            }
        }
        if let Some(ref s) = since_s {
            if m.date.as_str() < s.as_str() {
                return false;
            }
        }
        if let Some(ref u) = until_s {
            if m.date.as_str() > u.as_str() {
                return false;
            }
        }
        true
    };
    let mut sess_agg = store_query::DiskTimeFold::new()?;
    scan_messages_streaming(
        &home_dir,
        &clients,
        None,
        options.use_env_roots,
        &options.scanner_settings,
        &msg_filter,
        &mut |m: &UnifiedMessage| {
            sess_agg.feed(m);
        },
    )?;
    let (metrics, _) = sess_agg.finish(sessionize::DEFAULT_IDLE_GAP_MS)?;

    Ok(TimeMetricsReport {
        metrics,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}

pub async fn generate_graph(options: ReportOptions) -> Result<GraphResult, String> {
    let pricing = resolve_graph_pricing(GraphPricingMode::BestEffort, || async {
        pricing::PricingService::get_or_init().await.map(Some)
    })
    .await?;
    Ok(generate_graph_with_loaded_pricing(
        options,
        pricing.as_deref(),
        GraphPricingMode::BestEffort,
    )
    .await?
    .into_graph())
}

pub async fn generate_local_graph_report(options: ReportOptions) -> Result<GraphResult, String> {
    Ok(generate_local_graph_report_with_contract(options)
        .await?
        .into_graph())
}

/// Best-effort local graph with its immutable per-call pricing contract.
pub async fn generate_local_graph_report_with_contract(
    options: ReportOptions,
) -> Result<GraphResultWithContract, String> {
    let pricing = resolve_graph_pricing(GraphPricingMode::BestEffort, || async {
        Ok(load_pricing_for_local_parse().await)
    })
    .await?;
    generate_graph_with_loaded_pricing(options, pricing.as_deref(), GraphPricingMode::BestEffort)
        .await
}
/// Lean one-shot app graph: identical mapped wire payload, no sessionize fold.
pub async fn lean_local_graph_report(options: ReportOptions) -> Result<GraphResult, String> {
    let pricing = resolve_graph_pricing(GraphPricingMode::BestEffort, || async {
        Ok(load_pricing_for_local_parse().await)
    })
    .await?;
    generate_graph_with_loaded_pricing_inner(
        None,
        options,
        pricing.as_deref(),
        GraphPricingMode::BestEffort,
        true,
    )
    .await
    .map(GraphResultWithContract::into_graph)
}

pub async fn generate_local_graph_report_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
) -> Result<GraphResultWithContract, String> {
    let pricing = resolve_graph_pricing(GraphPricingMode::BestEffort, || async {
        Ok(load_pricing_for_local_parse_with_context(Some(context)).await)
    })
    .await?;
    generate_graph_with_loaded_pricing_inner(
        Some(context),
        options,
        pricing.as_deref(),
        GraphPricingMode::BestEffort,
        false,
    )
    .await
}

/// Generate the local graph without resolving or loading any pricing dataset.
/// Provider-reported message costs still flow through the existing scan and
/// aggregation path unchanged.
pub async fn generate_local_graph_report_local_only(
    options: ReportOptions,
) -> Result<GraphResultWithContract, String> {
    generate_graph_with_loaded_pricing(options, None, GraphPricingMode::LocalOnly).await
}

pub async fn generate_local_graph_report_local_only_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: ReportOptions,
) -> Result<GraphResultWithContract, String> {
    generate_graph_with_loaded_pricing_inner(
        Some(context),
        options,
        None,
        GraphPricingMode::LocalOnly,
        false,
    )
    .await
}

/// Streaming graph entry-point.
///
/// Accepts an already-parsed message slice, applies an optional `since`
/// date prefix filter (`msg.date.as_str() >= since`), then folds through
/// `StreamingAggregator` and wraps the result via
/// `aggregator::generate_graph_result`.
pub fn build_graph_result_from_messages(
    messages: &[UnifiedMessage],
    since: Option<&str>,
) -> GraphResult {
    build_graph_result_with_contract_from_messages(messages, since, GraphPricingMode::BestEffort)
        .into_graph()
}

fn build_graph_result_with_contract_from_messages(
    messages: &[UnifiedMessage],
    since: Option<&str>,
    pricing_mode: GraphPricingMode,
) -> GraphResultWithContract {
    let filtered = messages
        .iter()
        .filter(|msg| since.is_none_or(|s| msg.date.as_str() >= s));
    let cost_coverage = coverage_for_messages(filtered.clone());
    let contributions = aggregator::fold_messages_iter(filtered);
    GraphResultWithContract {
        graph: aggregator::generate_graph_result(contributions, 0),
        contract: GraphMetaContract {
            pricing_mode,
            cost_coverage,
        },
    }
}

fn is_headless_path(path: &Path, headless_roots: &[PathBuf]) -> bool {
    headless_roots.iter().any(|root| path.starts_with(root))
}

fn apply_headless_agent(message: &mut UnifiedMessage, is_headless: bool) {
    if is_headless && message.agent.is_none() {
        message.agent = Some("headless".to_string());
    }
}

fn pricing_multiplier(message: &UnifiedMessage) -> f64 {
    // Zed bills hosted models at provider list price + 10%.
    // Source: https://zed.dev/docs/ai/plans-and-usage and https://zed.dev/docs/ai/models
    //
    // The multiplier is keyed on the message's `provider_id`, not on the
    // provenance of the matched LiteLLM pricing row. Today this is safe because
    // tokscale's bundled LiteLLM dataset only carries upstream-provider rows
    // (anthropic, openai, google) for the underlying models. If a future
    // LiteLLM update adds rows under provider `zed.dev` that already include
    // Zed's markup, this function would double-bill — revisit by threading
    // the matched-price provenance through `apply_pricing_if_available`.
    if message.client == "zed"
        && message
            .provider_id
            .eq_ignore_ascii_case(sessions::zed::ZED_HOSTED_PROVIDER)
    {
        1.1
    } else {
        1.0
    }
}

fn apply_pricing_if_available(
    message: &mut UnifiedMessage,
    pricing: Option<&pricing::PricingService>,
) {
    if message.has_authoritative_cost() {
        return;
    }

    let Some(pricing) = pricing else {
        return;
    };

    let Some(estimate) = pricing.estimate_cost_with_provider(
        &message.model_id,
        Some(&message.provider_id),
        &message.tokens,
    ) else {
        return;
    };
    let calculated_cost = estimate.cost * pricing_multiplier(message);

    match estimate.coverage {
        pricing::lookup::EstimateCoverage::Complete => {
            message.cost = calculated_cost;
            message.mark_estimated_cost();
        }
        pricing::lookup::EstimateCoverage::Partial => {
            message.cost = calculated_cost;
            message.mark_partially_estimated_cost();
        }
        pricing::lookup::EstimateCoverage::None => {}
    }
}

/// Reprice a streaming-lane message, respecting an authoritative embedded cost.
///
/// Clients that record a real per-message cost of their own (e.g. MiMo Code,
/// which stores `usage.cost`) pass `guard_authoritative_cost = true`: the
/// message is repriced only when its embedded cost is absent (`<= 0.0`), so a
/// recomputed `tokens * rate` never clobbers the authoritative value if the
/// model later resolves to a price. Every other lane passes `false` and
/// reprices unconditionally (unchanged behaviour). `#742` Part 2.
fn reprice_lane_message(
    message: &mut UnifiedMessage,
    pricing: Option<&pricing::PricingService>,
    guard_authoritative_cost: bool,
) {
    if !guard_authoritative_cost || message.cost <= 0.0 {
        apply_pricing_if_available(message, pricing);
    }
}

fn select_local_parse_pricing<F>(
    fresh: Result<Arc<pricing::PricingService>, String>,
    stale: F,
) -> Option<Arc<pricing::PricingService>>
where
    F: FnOnce() -> Option<pricing::PricingService>,
{
    fresh.ok().or_else(|| stale().map(Arc::new))
}

async fn load_pricing_for_local_parse_with_context(
    context: Option<&ResolvedLocalSourceContext>,
) -> Option<Arc<pricing::PricingService>> {
    let cache_only = context
        .map(ResolvedLocalSourceContext::pricing_cache_only)
        .unwrap_or_else(|| {
            std::env::var("TOKSCALE_PRICING_CACHE_ONLY")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false)
        });
    if cache_only {
        return context
            .map(|context| {
                pricing::PricingService::load_cached_any_age_from_config_dir(
                    context.pricing_config_dir(),
                )
            })
            .unwrap_or_else(pricing::PricingService::load_cached_any_age)
            .map(Arc::new);
    }

    // Interactive/local views should pick up newly released model pricing as soon
    // as a fresh fetch succeeds, but still remain usable offline by falling back
    // to any cached dataset when the network path fails.
    select_local_parse_pricing(
        pricing::PricingService::get_or_init().await,
        pricing::PricingService::load_cached_any_age,
    )
}

async fn load_pricing_for_local_parse() -> Option<Arc<pricing::PricingService>> {
    load_pricing_for_local_parse_with_context(None).await
}

fn resolve_local_parse_request(
    options: &LocalParseOptions,
) -> Result<(String, Vec<String>), String> {
    let home_dir = get_home_dir_string(&options.home_dir)?;
    let clients = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|c| c.parse_local())
            .map(|c| c.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    });
    Ok((home_dir, clients))
}

fn parse_local_unified_messages_resolved(
    options: LocalParseOptions,
    home_dir: &str,
    clients: &[String],
    pricing: Option<&pricing::PricingService>,
) -> Result<Vec<UnifiedMessage>, String> {
    let messages = collect_local_unified_messages(
        home_dir,
        clients,
        pricing,
        options.use_env_roots,
        &options.scanner_settings,
        None,
    )?;
    Ok(filter_unified_messages(messages, &options))
}
/// Max mtime (unix ms) across every file the local scan would read. This
/// remains the timestamp-shaped probe used by pruning and diagnostics;
/// in-process report caches that must observe source deletion use
/// `local_source_change_token` instead. Database-backed sources contribute both
/// the db file and its `-wal` sidecar. Stat failures contribute nothing, so a
/// vanished file alone does not change this max-mtime value.
pub fn latest_source_mtime_ms(options: &LocalParseOptions) -> Result<u64, String> {
    let scan_result = scan_local_sources(options)?;
    Ok(latest_source_mtime_ms_from_scan(&scan_result))
}

fn scan_local_sources(options: &LocalParseOptions) -> Result<scanner::ScanResult, String> {
    let (home_dir, clients) = resolve_local_parse_request(options)?;
    let scan = scanner::scan_all_clients_with_scanner_settings(
        &home_dir,
        &clients,
        options.use_env_roots,
        &options.scanner_settings,
    );
    if let Some(error) = &scan.discovery_error {
        return Err(error.clone());
    }
    Ok(scan)
}

fn scan_local_sources_with_context(
    context: &ResolvedLocalSourceContext,
    options: &LocalParseOptions,
) -> Result<scanner::ScanResult, String> {
    let clients = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|client| client.parse_local())
            .map(|client| client.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    });
    let scan = scanner::scan_all_clients_with_source_context(context, &clients)
        .map_err(|error| error.to_string())?;
    if let Some(error) = &scan.discovery_error {
        return Err(error.clone());
    }
    Ok(scan)
}

fn latest_source_mtime_ms_from_scan(scan_result: &scanner::ScanResult) -> u64 {
    let mut latest: u64 = 0;
    for files in scan_result.files.iter() {
        for path in files {
            latest = latest.max(file_mtime_ms(path).unwrap_or(0));
        }
    }
    let mut dbs: Vec<PathBuf> = scan_result.opencode_dbs.clone();
    let single_dbs = [
        &scan_result.synthetic_db,
        &scan_result.kilo_db,
        &scan_result.goose_db,
        &scan_result.kiro_db,
        &scan_result.zcode_db,
    ];
    dbs.extend(single_dbs.into_iter().flatten().cloned());
    // Hermes/Zed dbs may also be auto-discovered or supplied through extra
    // scan roots (the `files` lanes) — use the plural helpers so every db gets
    // its `-wal` sidecar probed, not just the default-path single.
    dbs.extend(scan_result.hermes_db_paths());
    dbs.extend(scan_result.zed_db_paths());
    dbs.extend(scan_result.crush_dbs.iter().map(|c| c.db_path.clone()));
    // Antigravity CLI conversation `.db` files arrive via the generic `files`
    // lane (a `*.db` glob, no dedicated ScanResult field), so probe their `-wal`
    // sidecars here too — a WAL-only write would otherwise leave the change
    // token unchanged and the live tail would never re-parse the new usage.
    dbs.extend(scan_result.get(ClientId::AntigravityCli).iter().cloned());
    // micode `.db` files likewise arrive via the generic `*.db` glob and are
    // WAL-mode SQLite, so probe their `-wal` sidecars for the live-tail change
    // token too.
    dbs.extend(scan_result.get(ClientId::MiMoCode).iter().cloned());
    for db in dbs {
        latest = latest.max(file_mtime_ms(&db).unwrap_or(0));
        let mut wal = db.into_os_string();
        wal.push("-wal");
        latest = latest.max(file_mtime_ms(Path::new(&wal)).unwrap_or(0));
    }
    if let Some(db_path) = &scan_result.copilot_desktop_db {
        latest = latest.max(copilot_desktop_source_mtime_ms(db_path).unwrap_or(0));
    }
    // jcode snapshots (`session_*.json`) carry a sibling `.journal.jsonl`
    // append-log; jcode writes new turns there between snapshot rewrites,
    // leaving the snapshot's mtime untouched. The snapshot itself is already
    // covered by the `scan_result.files` loop above, but the journal is
    // deliberately excluded from the scan (the glob is `session_*.json`), so
    // probe it here — otherwise a journal-only append leaves the change token
    // unchanged and the live tail never re-parses the new usage.
    for snapshot in scan_result.get(ClientId::Jcode) {
        let journal = message_cache::jcode_journal_path(snapshot);
        latest = latest.max(file_mtime_ms(&journal).unwrap_or(0));
    }
    // Legacy Grok sessions reconcile sibling metadata that can change without
    // touching updates.jsonl. The self-contained unified log is already covered
    // by the primary-file scan above.
    for source in scan_result.get(ClientId::Grok) {
        if source.file_name().and_then(|name| name.to_str()) != Some("updates.jsonl") {
            continue;
        }
        if let Some(parent) = source.parent() {
            for sibling in message_cache::GROK_METADATA_SIBLINGS {
                latest = latest.max(file_mtime_ms(&parent.join(sibling)).unwrap_or(0));
            }
        }
    }
    // Roo-family task parsers read model and agent identity from the sibling
    // api_conversation_history.json. Probe it alongside ui_messages.json so a
    // history-only rewrite reaches the cache fingerprint and active lane.
    for client in [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline] {
        for ui_messages in scan_result.get(client) {
            latest = latest.max(roo_source_mtime_ms(ui_messages).unwrap_or(0));
        }
    }
    // These file-backed parsers consult secondary sources whose writes do not
    // update the scanned primary: Droid's fallback transcript, legacy Kimi's
    // shared config, and Kiro's CLI/IDE message sidecars. Probe each dependency
    // so a sibling-only change reaches the specialized cache fingerprint.
    for settings in scan_result.get(ClientId::Droid) {
        latest = latest.max(droid_source_mtime_ms(settings).unwrap_or(0));
    }
    for wire in scan_result.get(ClientId::Kimi) {
        latest = latest.max(kimi_source_mtime_ms(wire).unwrap_or(0));
    }
    for session in scan_result.get(ClientId::Kiro) {
        latest = latest.max(kiro_source_mtime_ms(session).unwrap_or(0));
    }
    latest
}

/// Opaque change token for in-process report caches. Hash the current source
/// topology and each parser dependency's size and nanosecond mtime so creating,
/// deleting, or rewriting a non-max source still invalidates cached graphs and
/// the live tail.
pub fn local_source_change_token(options: &LocalParseOptions) -> Result<u64, String> {
    let scan_result = scan_local_sources(options)?;
    local_source_change_token_inner(&scan_result)
}

pub fn local_source_change_token_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: &LocalParseOptions,
) -> Result<u64, String> {
    let _storage_management = disk_usage::ingestion_guard(context.usage_dir())?;
    usage_migration::prepare(context.usage_dir(), context.legacy_usage_roots())?;
    let scan_result = scan_local_sources_with_context(context, options)?;
    let scanner_token = local_source_change_token_inner(&scan_result)?;
    let usage_root = context.usage_dir().to_path_buf();
    let retained_token = message_cache::retained_state_change_token(
        Some(&usage_root),
        scan_result.get(ClientId::Claude),
        context.home_dir(),
    )?;
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scanner_token.hash(&mut hasher);
    retained_token.hash(&mut hasher);
    Ok(hasher.finish())
}

fn sqlite_source_paths(scan_result: &scanner::ScanResult) -> Vec<PathBuf> {
    let mut dbs = scan_result.opencode_dbs.clone();
    let single_dbs = [
        &scan_result.synthetic_db,
        &scan_result.kilo_db,
        &scan_result.goose_db,
        &scan_result.kiro_db,
        &scan_result.zcode_db,
    ];
    dbs.extend(single_dbs.into_iter().flatten().cloned());
    dbs.extend(scan_result.hermes_db_paths());
    dbs.extend(scan_result.zed_db_paths());
    dbs.extend(
        scan_result
            .crush_dbs
            .iter()
            .map(|source| source.db_path.clone()),
    );
    dbs.extend(scan_result.get(ClientId::AntigravityCli).iter().cloned());
    dbs.extend(scan_result.get(ClientId::MiMoCode).iter().cloned());
    dbs.extend(scan_result.copilot_desktop_db.iter().cloned());
    dbs.sort();
    dbs.dedup();
    dbs
}

fn local_source_change_token_inner(scan_result: &scanner::ScanResult) -> Result<u64, String> {
    use std::hash::{Hash, Hasher};

    let mut paths: Vec<PathBuf> = scan_result.files.iter().flatten().cloned().collect();
    let dbs = sqlite_source_paths(scan_result);
    let mut sqlite_wals = HashSet::new();
    for db in dbs {
        paths.push(db.clone());
        let mut wal = db.into_os_string();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        sqlite_wals.insert(wal.clone());
        paths.push(wal);
    }
    if let Some(db_path) = &scan_result.copilot_desktop_db {
        probe_readable_dependency(db_path, false).map_err(|error| {
            format!(
                "Failed to read Copilot Desktop database {}: {error}",
                db_path.display()
            )
        })?;
        paths.push(db_path.clone());
        let mut wal = db_path.clone().into_os_string();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        probe_readable_dependency(&wal, true).map_err(|error| {
            format!(
                "Failed to read Copilot Desktop WAL {}: {error}",
                wal.display()
            )
        })?;
        paths.push(wal);
        paths.extend(
            sessions::copilot_desktop::session_state_event_paths(db_path).map_err(|error| {
                format!(
                    "Failed to probe Copilot Desktop session-state dependencies for {}: {error}",
                    db_path.display()
                )
            })?,
        );
    }

    for snapshot in scan_result.get(ClientId::Jcode) {
        paths.push(message_cache::jcode_journal_path(snapshot));
    }
    for source in scan_result.get(ClientId::Grok) {
        if source.file_name().and_then(|name| name.to_str()) == Some("updates.jsonl") {
            if let Some(parent) = source.parent() {
                paths.extend(
                    message_cache::GROK_METADATA_SIBLINGS
                        .into_iter()
                        .map(|name| parent.join(name)),
                );
            }
        }
    }
    for client in [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline] {
        paths.extend(
            scan_result
                .get(client)
                .iter()
                .map(|path| sessions::roocode::history_path_for_ui_messages(path)),
        );
    }
    paths.extend(
        scan_result
            .get(ClientId::Droid)
            .iter()
            .filter_map(|path| sessions::droid::droid_jsonl_path(path)),
    );
    paths.extend(
        scan_result
            .get(ClientId::Kimi)
            .iter()
            .filter_map(|path| sessions::kimi::kimi_config_path(path)),
    );
    paths.extend(
        scan_result
            .get(ClientId::Kiro)
            .iter()
            .filter_map(|path| sessions::kiro::kiro_related_messages_path(path)),
    );

    paths.sort();
    paths.dedup();

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in paths {
        path.hash(&mut hasher);
        let state = match std::fs::metadata(&path) {
            Ok(metadata) => {
                let sqlite_wal = sqlite_wals.contains(&path);
                let metadata = if sqlite_wal {
                    std::fs::File::open(&path)
                        .and_then(|mut file| {
                            use std::io::Read;
                            let mut byte = [0];
                            let count = file.read(&mut byte)?;
                            let metadata = file.metadata()?;
                            if (metadata.len() == 0) != (count == 0) {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "SQLite WAL changed during readability probe",
                                ));
                            }
                            Ok(metadata)
                        })
                        .map_err(|_| "SQLite WAL readability probe failed")?
                } else {
                    metadata
                };
                // Empty SQLite WALs contain no committed frames. Do not apply
                // this policy to generic source siblings or unreadable files.
                if sqlite_wal && metadata.len() == 0 {
                    None
                } else {
                    let modified = metadata
                        .modified()
                        .map_err(|_| "source metadata probe failed")?
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|duration| (duration.as_secs(), duration.subsec_nanos()));
                    Some((metadata.len(), modified))
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err("source metadata probe failed".to_string()),
        };
        state.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn probe_readable_dependency(path: &Path, optional: bool) -> std::io::Result<()> {
    match std::fs::File::open(path) {
        Ok(_) => Ok(()),
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// File mtime as unix ms; `None` on any stat failure.
fn file_mtime_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as u64)
}

/// Newest mtime for a primary source and every related path its parser reads.
/// Missing optional paths are ignored. Any other metadata failure returns
/// `None`, allowing pruning callers to fail open rather than silently dropping
/// a source whose freshness cannot be established.
fn source_with_related_mtime_ms(
    source: &Path,
    related_paths: impl IntoIterator<Item = PathBuf>,
) -> Option<u64> {
    let mut latest = file_mtime_ms(source)?;
    for related in related_paths {
        let metadata = match std::fs::metadata(&related) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let modified = metadata.modified().ok()?;
        let duration = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        latest = latest.max(duration.as_millis() as u64);
    }
    Some(latest)
}

fn roo_source_mtime_ms(ui_messages_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        ui_messages_path,
        std::iter::once(sessions::roocode::history_path_for_ui_messages(
            ui_messages_path,
        )),
    )
}

fn droid_source_mtime_ms(settings_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        settings_path,
        sessions::droid::droid_jsonl_path(settings_path),
    )
}

fn kimi_source_mtime_ms(wire_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(wire_path, sessions::kimi::kimi_config_path(wire_path))
}

fn kiro_source_mtime_ms(session_path: &Path) -> Option<u64> {
    source_with_related_mtime_ms(
        session_path,
        sessions::kiro::kiro_related_messages_path(session_path),
    )
}

fn copilot_desktop_source_mtime_ms(db_path: &Path) -> Option<u64> {
    probe_readable_dependency(db_path, false).ok()?;
    let mut wal = db_path.to_path_buf().into_os_string();
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    probe_readable_dependency(&wal, true).ok()?;
    let events = sessions::copilot_desktop::session_state_event_paths(db_path).ok()?;
    source_with_related_mtime_ms(db_path, std::iter::once(wal).chain(events))
}

/// Newest mtime of an Antigravity CLI db and its `<db>-wal` (a missing `-wal` is
/// ignored), or `None` to keep the db without a decision: a symlinked db, whose
/// WAL sits beside the link or the target depending on the writer's SQLite
/// build, or any other stat failure. `-wal` is read before the db because a
/// closing checkpoint writes the db and then unlinks `-wal`: reading the db
/// second, a just-removed `-wal` cannot leave it looking older than that write.
/// A read-only open creates an empty `-wal` and `-shm` when none exists
/// (observed with SQLite 3.53.4), which only keeps a db longer and moves the
/// change token once.
fn antigravity_cli_db_mtime_ms(db: &Path) -> Option<u64> {
    let ms = |meta: std::fs::Metadata| -> Option<u64> {
        let since_epoch = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(since_epoch.as_millis() as u64)
    };
    let wal = match std::fs::metadata(message_cache::append_path_suffix(db, "-wal")) {
        Ok(meta) => Some(ms(meta)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    if std::fs::symlink_metadata(db).ok()?.file_type().is_symlink() {
        return None;
    }
    let db_ms = ms(std::fs::metadata(db).ok()?)?;
    Some(wal.map_or(db_ms, |wal_ms| wal_ms.max(db_ms)))
}

/// Newest mtime for a Grok source and every file that affects its parse. The
/// unified log is self-contained; legacy updates include metadata siblings kept
/// in lockstep with `SourceFingerprint::from_grok_path`. `None` keeps the source
/// during pruning because over-parsing is safer than silently dropping usage.
fn grok_source_mtime_ms(source_path: &Path) -> Option<u64> {
    let mut latest = file_mtime_ms(source_path)?;
    if source_path.file_name().and_then(|name| name.to_str()) == Some("unified.jsonl") {
        return Some(latest);
    }

    let Some(parent) = source_path.parent() else {
        return Some(latest);
    };
    for name in message_cache::GROK_METADATA_SIBLINGS {
        let sibling = parent.join(name);
        if sibling.exists() {
            latest = latest.max(file_mtime_ms(&sibling)?);
        }
    }
    Some(latest)
}

/// Drop file-backed session logs older than `threshold_ms` (unix ms, mtime)
/// from a scan. Sources whose freshness is not captured by their scanned
/// file's own mtime are left untouched, because a sibling can change without
/// touching it: the Hermes/Zed/micode lanes hold SQLite dbs (WAL writes may
/// not bump the main `.db` mtime), while the jcode lane holds a
/// `session_*.json` snapshot whose sibling `.journal.jsonl` is appended between
/// snapshot rewrites. Those lanes remain exempt. Roo-family, Droid, legacy Kimi,
/// Kiro file sources, Grok sources, and Antigravity CLI dbs can still be
/// bounded by folding every parser dependency into their newest mtime.
///
/// Antigravity CLI retains symlinked dbs and every db on Windows; otherwise
/// its db and WAL are checked in checkpoint-safe order.
/// Any stat failure keeps the file — over-parsing is safe, silently skipping is
/// not.
fn prune_scan_result_by_mtime(scan_result: &mut scanner::ScanResult, threshold_ms: u64) {
    let copilot_desktop_fresh = scan_result.copilot_desktop_db.as_ref().is_some_and(|path| {
        copilot_desktop_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
    });
    if !copilot_desktop_fresh {
        scan_result.copilot_desktop_db = None;
    }

    // Lanes whose scanned file's mtime does not reflect a sibling write
    // (SQLite `-wal` or jcode's `.journal.jsonl`); kept in lockstep with the
    // sibling probes in `latest_source_mtime_ms`.
    let db_lanes = [
        ClientId::Hermes as usize,
        ClientId::Zed as usize,
        ClientId::MiMoCode as usize,
        ClientId::Jcode as usize,
    ];
    let roo_lanes = [
        ClientId::RooCode as usize,
        ClientId::KiloCode as usize,
        ClientId::Cline as usize,
    ];
    for (lane, files) in scan_result.files.iter_mut().enumerate() {
        if lane == ClientId::Copilot as usize && copilot_desktop_fresh {
            // A fresh Desktop aggregate may overlap any older OTEL row from the
            // same session. Keep the complete OTEL cohort so the shared selector
            // cannot reopen Desktop usage that a full scan suppresses.
            continue;
        }
        if db_lanes.contains(&lane) {
            continue;
        }
        if lane == ClientId::AntigravityCli as usize {
            if cfg!(windows) {
                continue;
            }
            files.retain(|path| {
                antigravity_cli_db_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
            });
            continue;
        }
        if roo_lanes.contains(&lane) {
            files
                .retain(|path| roo_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms));
            continue;
        }
        if lane == ClientId::Droid as usize {
            files.retain(|path| {
                droid_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
            });
            continue;
        }
        if lane == ClientId::Kimi as usize {
            files.retain(|path| {
                kimi_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
            });
            continue;
        }
        if lane == ClientId::Kiro as usize {
            // globalStorage precedence crosses files. If any IDE source changed,
            // retain the complete IDE cohort so an older execution can still
            // suppress a newer snapshot; CLI sources remain independently prunable.
            let keep_global_storage_batch = files
                .iter()
                .filter(|path| sessions::kiro::is_kiro_global_storage_source(path))
                .any(|path| kiro_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms));
            files.retain(|path| {
                if sessions::kiro::is_kiro_global_storage_source(path) {
                    keep_global_storage_batch
                } else {
                    kiro_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms)
                }
            });
            continue;
        }
        if lane == ClientId::Grok as usize {
            let is_unified = |path: &Path| {
                path.file_name().and_then(|name| name.to_str()) == Some("unified.jsonl")
            };
            let is_fresh =
                |path: &Path| grok_source_mtime_ms(path).is_none_or(|mtime| mtime >= threshold_ms);
            let unified_fresh = files.iter().any(|path| is_unified(path) && is_fresh(path));
            if unified_fresh {
                // A fresh global authority file can cover any legacy session and
                // also needs those rows for workspace attribution.
                continue;
            }

            let legacy_fresh = files.iter().any(|path| !is_unified(path) && is_fresh(path));
            files.retain(|path| {
                if is_unified(path) {
                    // An older authority file must remain available while a
                    // legacy session is fresh, or live-tail pruning can reopen
                    // rows that full reports correctly suppress.
                    legacy_fresh
                } else {
                    is_fresh(path)
                }
            });
            continue;
        }
        files.retain(|path| {
            let Ok(meta) = std::fs::metadata(path) else {
                return true;
            };
            let Ok(modified) = meta.modified() else {
                return true;
            };
            let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) else {
                return true;
            };
            duration.as_millis() as u64 >= threshold_ms
        });
    }
}

/// Visit only recent local messages, with the identity and selection semantics
/// of `parse_local_clients`. No whole-history `ParsedMessage` vector is built.
///
/// `cutoff_ms` is inclusive Unix milliseconds. Date and timestamp filters run
/// after source authority and dedup selection, just like the materialized API;
/// report APIs retain their own query-dependent OpenCode fallback semantics.
/// This synchronous token/trace path performs no pricing network requests.
/// Source rows, retained/live precedence, and exact cross-source selection use
/// the canonical disk cache and file-backed query state. The recent cutoff
/// filters selected rows, not source ingestion or SQLite source timestamps.
pub fn visit_recent_local_messages<S>(
    options: LocalParseOptions,
    cutoff_ms: i64,
    sink: &mut S,
) -> Result<(), String>
where
    S: FnMut(&UnifiedMessage),
{
    let (home_dir, clients) = resolve_local_parse_request(&options)?;
    let year_prefix = options.year.as_ref().map(|year| format!("{year}-"));
    let context = ResolvedLocalSourceContext::capture(
        Some(PathBuf::from(&home_dir)),
        options.use_env_roots,
        options.scanner_settings.clone(),
    )
    .map_err(|error| error.to_string())?;
    let (snapshot, query) = ingest_local_usage_inner(
        &context,
        &clients,
        options.modified_after,
        LocalUsageSelection::LocalClients,
        true,
    )?;
    fold_prepared_usage_snapshot(
        &snapshot,
        &query,
        None,
        LocalUsageQuery {
            from_ms: Some(cutoff_ms),
            ..LocalUsageQuery::default()
        },
        &|message| {
            year_prefix
                .as_ref()
                .is_none_or(|prefix| message.date.starts_with(prefix))
                && options
                    .since
                    .as_ref()
                    .is_none_or(|since| message.date >= *since)
                && options
                    .until
                    .as_ref()
                    .is_none_or(|until| message.date <= *until)
        },
        sink,
        true,
    )
    .map(|_| ())
}

pub fn parse_local_clients(options: LocalParseOptions) -> Result<ParsedMessages, String> {
    parse_local_clients_inner(None, options)
}

pub fn parse_local_clients_with_source_context(
    context: &ResolvedLocalSourceContext,
    options: LocalParseOptions,
) -> Result<ParsedMessages, String> {
    parse_local_clients_inner(Some(context), options)
}

fn parse_local_clients_inner(
    context: Option<&ResolvedLocalSourceContext>,
    options: LocalParseOptions,
) -> Result<ParsedMessages, String> {
    let start = Instant::now();
    let home = context
        .map(|context| context.home_dir().to_path_buf())
        .map(Ok)
        .unwrap_or_else(|| get_home_dir_string(&options.home_dir).map(PathBuf::from))?;
    let clients = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|client| client.parse_local())
            .map(|client| client.as_str().to_string())
            .collect();
        clients.push("synthetic".to_string());
        clients
    });
    let mut messages = Vec::new();
    let counts = scan_messages_disk(
        context,
        &home,
        &clients,
        None,
        options.use_env_roots,
        &options.scanner_settings,
        options.modified_after,
        store_query::SelectionMode::LocalClients,
        &|_| true,
        &mut |message| messages.push(unified_to_parsed(message)),
    )?;
    Ok(ParsedMessages {
        messages: filter_parsed_messages(messages, &options),
        counts,
        processing_time_ms: start.elapsed().as_millis() as u32,
    })
}
#[doc(hidden)]
pub async fn parse_local_unified_messages_with_pricing(
    options: LocalParseOptions,
    pricing: Option<&pricing::PricingService>,
) -> Result<Vec<UnifiedMessage>, String> {
    let (home_dir, clients) = resolve_local_parse_request(&options)?;
    parse_local_unified_messages_resolved(options, &home_dir, &clients, pricing)
}

/// Parse the local unified message stream into a fully materialized `Vec`.
///
/// Compatibility selection intentionally differs from report selection for
/// some client lanes. Storage and ingestion are shared, but only this explicit
/// materialized API collects the complete selected message history. Reports
/// and recent visitors fold the disk-selected stream directly.
pub async fn parse_local_unified_messages(
    options: LocalParseOptions,
) -> Result<Vec<UnifiedMessage>, String> {
    let (home_dir, clients) = resolve_local_parse_request(&options)?;
    let pricing = load_pricing_for_local_parse().await;
    parse_local_unified_messages_resolved(options, &home_dir, &clients, pricing.as_deref())
}

fn unified_to_parsed(msg: &UnifiedMessage) -> ParsedMessage {
    ParsedMessage {
        client: msg.client.clone(),
        model_id: msg.model_id.clone(),
        provider_id: msg.provider_id.clone(),
        session_id: msg.session_id.clone(),
        workspace_key: msg.workspace_key.clone(),
        workspace_label: msg.workspace_label.clone(),
        timestamp: msg.timestamp,
        date: msg.date.clone(),
        input: msg.tokens.input,
        output: msg.tokens.output,
        cache_read: msg.tokens.cache_read,
        cache_write: msg.tokens.cache_write,
        reasoning: msg.tokens.reasoning,
        duration_ms: msg.duration_ms,
        message_count: msg.message_count,
        agent: msg.agent.clone(),
    }
}

fn filter_parsed_messages(
    messages: Vec<ParsedMessage>,
    options: &LocalParseOptions,
) -> Vec<ParsedMessage> {
    let mut filtered = messages;

    if let Some(year) = &options.year {
        let year_prefix = format!("{}-", year);
        filtered.retain(|m| m.date.starts_with(&year_prefix));
    }

    if let Some(since) = &options.since {
        filtered.retain(|m| m.date.as_str() >= since.as_str());
    }

    if let Some(until) = &options.until {
        filtered.retain(|m| m.date.as_str() <= until.as_str());
    }

    filtered
}

pub fn parsed_to_unified(msg: &ParsedMessage, cost: f64) -> UnifiedMessage {
    UnifiedMessage {
        client: msg.client.clone(),
        model_id: msg.model_id.clone(),
        provider_id: msg.provider_id.clone(),
        session_id: msg.session_id.clone(),
        workspace_key: msg.workspace_key.clone(),
        workspace_label: msg.workspace_label.clone(),
        timestamp: msg.timestamp,
        date: msg.date.clone(),
        tokens: TokenBreakdown {
            input: msg.input,
            output: msg.output,
            cache_read: msg.cache_read,
            cache_write: msg.cache_write,
            reasoning: msg.reasoning,
            cache_write_1h: 0,
        },
        cost,
        cost_source: CostSource::Unknown,
        duration_ms: msg.duration_ms,
        message_count: msg.message_count,
        agent: msg.agent.clone(),
        dedup_key: None,
        dedup_aliases: Vec::new(),
        is_turn_start: false,
        input_tokens_are_cache_split: false,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::copilot_desktop_source_mtime_ms;
    use super::{
        agent_bucket_key, aggregate_model_usage_entries, apply_pricing_if_available,
        canonical_model_id, clear_model_aliases, coverage_for_messages,
        dedupe_latest_trae_messages, fold_messages_streaming, get_agents_report, get_hourly_report,
        get_model_report, get_model_report_with_source_context, get_monthly_report,
        get_window_usage, latest_source_mtime_ms, load_pricing_for_local_parse_with_context,
        local_source_change_token, local_source_change_token_with_source_context, message_cache,
        model_alias_generation, normalize_model_for_grouping, normalize_syntactic,
        opencode_authoritative_sources, opencode_identity_group,
        parse_all_messages_with_pricing_with_env_strategy, parse_local_clients,
        parse_local_clients_with_source_context, parse_local_unified_messages, parsed_to_unified,
        pricing, prune_scan_result_by_mtime, register_usage_data_invalidation_hook,
        reprice_lane_message, resolve_graph_pricing, retain_for_requested_clients, scanner,
        select_local_parse_pricing, sessions, set_model_aliases, snapshot_grouping_aliases,
        unified_to_parsed, AgentAccumulator, ClientId, CostCoverage, CostSource, GraphPricingMode,
        GroupBy, LocalParseOptions, ModelAliasMap, OpenCodeSelection, OpenCodeSourceIdentity,
        ReportOptions, ResolvedLocalSourceContext, TokenBreakdown, UnifiedMessage,
        RETAIN_OBSERVED_MESSAGES_CALLS, UNKNOWN_WORKSPACE_LABEL,
    };
    use bincode::Options;
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::io::Write;

    fn scan_messages_streaming<F, S>(
        home: &str,
        clients: &[String],
        pricing: Option<&pricing::PricingService>,
        use_env: bool,
        settings: &scanner::ScannerSettings,
        filter: &F,
        sink: &mut S,
    ) where
        F: Fn(&UnifiedMessage) -> bool,
        S: FnMut(&UnifiedMessage),
    {
        let context = ResolvedLocalSourceContext::capture(
            Some(PathBuf::from(home)),
            use_env,
            settings.clone(),
        )
        .unwrap();
        let (snapshot, query) = super::ingest_local_usage_inner(
            &context,
            clients,
            None,
            super::LocalUsageSelection::Report,
            true,
        )
        .expect("fixture source ingestion succeeds");
        super::fold_prepared_usage_snapshot(
            &snapshot,
            &query,
            pricing,
            super::LocalUsageQuery::default(),
            filter,
            sink,
            true,
        )
        .expect("fixture snapshot query succeeds");
    }
    use std::path::{Path, PathBuf};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A payload written before the 1h/5m split lacks the key entirely, and a
    /// reader that rejects it would break every stored snapshot at once. This
    /// is separate from the bincode path, which `CACHE_FORMAT_VERSION` 4 and
    /// `mod format3` handle by decoding the old layout rather than defaulting.
    #[test]
    fn token_breakdown_decodes_json_written_before_the_1h_split() {
        let before = r#"{"input":1,"output":2,"cache_read":3,"cache_write":4,"reasoning":5}"#;
        let decoded: TokenBreakdown =
            serde_json::from_str(before).expect("pre-split JSON must still decode");
        assert_eq!(decoded.cache_write, 4, "existing lanes must survive");
        assert_eq!(
            decoded.cache_write_1h, 0,
            "a payload that predates the split reports no 1h portion"
        );
        assert_eq!(
            decoded.total(),
            15,
            "total is unchanged: cache_write is still the whole write"
        );
    }

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| (*key, std::env::var_os(key)))
                    .collect(),
            )
        }

        fn set(vars: &[(&'static str, &std::ffi::OsStr)]) -> Self {
            let keys: Vec<_> = vars.iter().map(|(key, _)| *key).collect();
            let guard = Self::capture(&keys);
            unsafe {
                for (key, value) in vars {
                    std::env::set_var(key, value);
                }
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn get_home_dir_string_prefers_non_empty_explicit_home() {
        let env_home = tempfile::TempDir::new().unwrap();
        let explicit_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[("HOME", env_home.path().as_os_str())]);
        let explicit = explicit_home.path().to_string_lossy().into_owned();

        assert_eq!(
            super::get_home_dir_string(&Some(explicit.clone())).unwrap(),
            explicit
        );
    }

    #[test]
    #[serial_test::serial]
    fn get_home_dir_string_empty_explicit_falls_through_platform_home() {
        let env_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[("HOME", env_home.path().as_os_str())]);
        let expected = env_home.path().to_string_lossy().into_owned();

        assert_eq!(super::get_home_dir_string(&None).unwrap(), expected);
        assert_eq!(
            super::get_home_dir_string(&Some(String::new())).unwrap(),
            expected
        );
    }

    fn scanner_fixture_path(home: &Path, relative: &str) -> PathBuf {
        #[cfg(windows)]
        {
            // The production scanner builds these roots with format!("{home}/{relative}").
            // Match that lexical path on Windows so cache assertions use the same key.
            PathBuf::from(format!("{}/{}", home.display(), relative))
        }
        #[cfg(not(windows))]
        {
            home.join(relative)
        }
    }

    fn opencode_test_env(cache_home: &Path, source_home: &Path) -> EnvGuard {
        let xdg_data_home = scanner_fixture_path(source_home, ".local/share");
        EnvGuard::set(&[
            ("HOME", cache_home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
            ("XDG_DATA_HOME", xdg_data_home.as_os_str()),
        ])
    }

    fn parse_all_messages_with_pricing(
        home_dir: &str,
        clients: &[String],
        pricing: Option<&pricing::PricingService>,
    ) -> Vec<UnifiedMessage> {
        parse_all_messages_with_pricing_with_env_strategy(
            home_dir,
            clients,
            pricing,
            false,
            &scanner::ScannerSettings::default(),
            None,
        )
    }

    fn graph_test_message(tokens: i64, cost: f64, source: CostSource) -> UnifiedMessage {
        let mut message = UnifiedMessage::new(
            "claude",
            "claude-sonnet-4-5",
            "anthropic",
            "session-1",
            1_751_000_000_000,
            TokenBreakdown {
                input: tokens,
                ..Default::default()
            },
            cost,
        );
        message.cost_source = source;
        message
    }

    #[tokio::test]
    async fn graph_pricing_resolver_skips_local_loader_and_calls_best_effort_once() {
        let local_calls = Arc::new(AtomicUsize::new(0));
        let local_calls_for_loader = Arc::clone(&local_calls);
        let local = resolve_graph_pricing(GraphPricingMode::LocalOnly, move || async move {
            local_calls_for_loader.fetch_add(1, Ordering::SeqCst);
            panic!("local-only must not invoke pricing loader");
        })
        .await
        .unwrap();
        assert!(local.is_none());
        assert_eq!(local_calls.load(Ordering::SeqCst), 0);

        let best_effort_calls = Arc::new(AtomicUsize::new(0));
        let best_effort_calls_for_loader = Arc::clone(&best_effort_calls);
        let best_effort = resolve_graph_pricing(GraphPricingMode::BestEffort, move || async move {
            best_effort_calls_for_loader.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        })
        .await
        .unwrap();
        assert!(best_effort.is_none());
        assert_eq!(best_effort_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[serial_test::serial]
    fn trae_streaming_flush_applies_best_effort_pricing_after_latest_selection() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let trae_dir = PathBuf::from(
            ClientId::Trae
                .data()
                .resolve_path_with_env_strategy(source_home.path().to_str().unwrap(), false),
        );
        std::fs::create_dir_all(&trae_dir).unwrap();
        std::fs::write(
            trae_dir.join("sessions.json"),
            serde_json::json!([{
                "model_name": "GPT-5.4",
                "session_id": "missing-cost",
                "usage_time": 1_776_000_000,
                "extra_info": {
                    "input_token": 10,
                    "output_token": 5,
                    "cache_read_token": 0,
                    "cache_write_token": 0
                }
            }])
            .to_string(),
        )
        .unwrap();

        let clients = ["trae".to_string()];
        let mut local = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| local.push(message.clone()),
        );
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].cost, 0.0);
        assert_eq!(local[0].cost_source, CostSource::Unknown);

        let pricing = pricing::PricingService::new(
            HashMap::from([(
                "gpt-5.4".to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.01),
                    output_cost_per_token: Some(0.02),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        let mut best_effort = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            Some(&pricing),
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| best_effort.push(message.clone()),
        );
        assert_eq!(best_effort.len(), 1);
        assert!((best_effort[0].cost - 0.2).abs() < 1e-9);
        assert_eq!(best_effort[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn graph_cost_coverage_folds_cost_source_after_relevance_filter() {
        let provider = graph_test_message(0, 0.0, CostSource::ProviderReported);
        let estimated = graph_test_message(10, 0.0, CostSource::Estimated);
        let partially_estimated = graph_test_message(10, 0.0, CostSource::PartiallyEstimated);
        let unknown = graph_test_message(10, 0.0, CostSource::Unknown);
        let mut message_only = graph_test_message(0, 0.0, CostSource::Unknown);
        message_only.message_count = 1;
        let mut structural = graph_test_message(0, 0.0, CostSource::Unknown);
        structural.message_count = 0;

        assert_eq!(
            coverage_for_messages(std::iter::empty()),
            CostCoverage::Complete
        );
        assert_eq!(coverage_for_messages([&provider]), CostCoverage::Complete);
        assert_eq!(coverage_for_messages([&estimated]), CostCoverage::Complete);
        assert_eq!(
            coverage_for_messages([&partially_estimated]),
            CostCoverage::Partial
        );
        assert_eq!(coverage_for_messages([&unknown]), CostCoverage::None);
        assert_eq!(coverage_for_messages([&message_only]), CostCoverage::None);
        assert_eq!(
            coverage_for_messages([&estimated, &unknown]),
            CostCoverage::Partial
        );
        assert_eq!(coverage_for_messages([&structural]), CostCoverage::Complete);
    }

    #[test]
    fn local_only_keeps_provider_reported_cost_without_pricing() {
        let mut message = graph_test_message(10, 0.75, CostSource::ProviderReported);
        apply_pricing_if_available(&mut message, None);
        assert_eq!(message.cost, 0.75);
        assert_eq!(message.cost_source, CostSource::ProviderReported);
    }

    #[test]
    fn local_and_priced_graphs_keep_the_same_topology() {
        let provider = graph_test_message(10, 0.75, CostSource::ProviderReported);
        let unknown = graph_test_message(20, 0.0, CostSource::Unknown);
        let local_messages = vec![provider.clone(), unknown.clone()];

        let mut priced_unknown = unknown;
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        apply_pricing_if_available(&mut priced_unknown, Some(&pricing));
        let priced_messages = vec![provider, priced_unknown];

        let local = super::build_graph_result_with_contract_from_messages(
            &local_messages,
            None,
            GraphPricingMode::LocalOnly,
        );
        let local_contract = local.contract();
        let priced = super::build_graph_result_with_contract_from_messages(
            &priced_messages,
            None,
            GraphPricingMode::BestEffort,
        );
        let priced_contract = priced.contract();

        assert_eq!(local_contract.pricing_mode, GraphPricingMode::LocalOnly);
        assert_eq!(local_contract.cost_coverage, CostCoverage::Partial);
        assert_eq!(priced_contract.cost_coverage, CostCoverage::Complete);
        assert_eq!(
            local.graph().contributions.len(),
            priced.graph().contributions.len()
        );
        for (local_day, priced_day) in local
            .graph()
            .contributions
            .iter()
            .zip(&priced.graph().contributions)
        {
            assert_eq!(local_day.date, priced_day.date);
            assert_eq!(local_day.totals.tokens, priced_day.totals.tokens);
            assert_eq!(local_day.totals.messages, priced_day.totals.messages);
            assert_eq!(local_day.token_breakdown, priced_day.token_breakdown);
            assert_eq!(local_day.turns_by_client, priced_day.turns_by_client);
            let local_topology: Vec<_> = local_day
                .clients
                .iter()
                .map(|client| {
                    (
                        client.client.as_str(),
                        client.model_id.as_str(),
                        client.provider_id.as_str(),
                        client.tokens.clone(),
                        client.messages,
                    )
                })
                .collect();
            let priced_topology: Vec<_> = priced_day
                .clients
                .iter()
                .map(|client| {
                    (
                        client.client.as_str(),
                        client.model_id.as_str(),
                        client.provider_id.as_str(),
                        client.tokens.clone(),
                        client.messages,
                    )
                })
                .collect();
            assert_eq!(local_topology, priced_topology);
        }
        assert!(priced.graph().summary.total_cost > local.graph().summary.total_cost);
        assert_eq!(local.graph().contributions[0].clients[0].cost, 0.75);
    }

    #[test]
    fn mux_mixed_cost_provenance_preserves_known_subtotal_and_message_count() {
        let source_dir = tempfile::TempDir::new().unwrap();
        let source = source_dir.path().join("session-usage.json");
        std::fs::write(
            &source,
            r#"{
                "version": 1,
                "byModel": {
                    "anthropic:claude-opus-4-6": {
                        "input": { "tokens": 100, "cost_usd": 0.1 },
                        "output": { "tokens": 50 }
                    }
                },
                "lastRequest": { "timestamp": 1700000000000 }
            }"#,
        )
        .unwrap();

        let raw = sessions::mux::parse_mux_file(&source);
        assert_eq!(raw.len(), 2);
        assert_eq!(
            raw.iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            150
        );
        assert_eq!(
            raw.iter().map(|message| message.message_count).sum::<i32>(),
            1
        );

        let local = super::build_graph_result_with_contract_from_messages(
            &raw,
            None,
            GraphPricingMode::LocalOnly,
        );
        assert_eq!(local.contract().cost_coverage, CostCoverage::Partial);
        assert_eq!(local.graph().summary.total_tokens, 150);
        assert_eq!(local.graph().summary.total_cost, 0.1);
        assert_eq!(local.graph().contributions[0].totals.messages, 1);

        let pricing = pricing::PricingService::new(
            HashMap::from([(
                "claude-opus-4-6".to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.01),
                    output_cost_per_token: Some(0.02),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        let mut priced = raw.clone();
        for message in &mut priced {
            apply_pricing_if_available(message, Some(&pricing));
        }
        let known = priced
            .iter()
            .find(|message| message.cost_source == CostSource::ProviderReported)
            .unwrap();
        let unknown = priced
            .iter()
            .find(|message| message.cost_source == CostSource::Estimated)
            .unwrap();
        assert_eq!(known.cost, 0.1);
        assert_eq!(unknown.cost, 1.0);
        assert_eq!(known.tokens.input, 100);
        assert_eq!(unknown.tokens.output, 50);

        let best_effort = super::build_graph_result_with_contract_from_messages(
            &priced,
            None,
            GraphPricingMode::BestEffort,
        );
        assert_eq!(best_effort.contract().cost_coverage, CostCoverage::Complete);
        assert_eq!(best_effort.graph().summary.total_tokens, 150);
        assert_eq!(best_effort.graph().summary.total_cost, 1.1);
        assert_eq!(best_effort.graph().contributions[0].totals.messages, 1);
    }

    #[test]
    fn graph_contract_is_bound_to_each_result_after_interleaving() {
        let message = graph_test_message(10, 0.75, CostSource::ProviderReported);
        let local = super::build_graph_result_with_contract_from_messages(
            std::slice::from_ref(&message),
            None,
            GraphPricingMode::LocalOnly,
        );
        let best_effort = super::build_graph_result_with_contract_from_messages(
            &[message],
            None,
            GraphPricingMode::BestEffort,
        );

        let serialized_local = serde_json::to_value(local.graph()).unwrap();
        assert!(serialized_local["meta"].get("pricingMode").is_none());
        assert_eq!(local.contract().pricing_mode, GraphPricingMode::LocalOnly);
        assert_eq!(local.contract().cost_coverage, CostCoverage::Complete);
        assert_eq!(
            best_effort.contract().pricing_mode,
            GraphPricingMode::BestEffort
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn public_local_graph_contract_preserves_fixture_topology_and_provider_cost() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let gjc_dir = source_home.path().join(".gjc/agent/sessions");
        std::fs::create_dir_all(&gjc_dir).unwrap();
        std::fs::write(
            gjc_dir.join("test.jsonl"),
            concat!(
                "{\"type\":\"session\",\"id\":\"gjc_ses_001\",\"cwd\":\"/work/pi\"}\n",
                "{\"type\":\"message\",\"id\":\"provider\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4\",\"provider\":\"anthropic\",\"timestamp\":1767225601000,\"usage\":{\"input\":100,\"output\":50,\"cost\":{\"total\":0.3}}}}\n",
                "{\"type\":\"message\",\"id\":\"unknown\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4\",\"provider\":\"anthropic\",\"timestamp\":1767225661000,\"usage\":{\"input\":200,\"output\":60}}}\n",
            ),
        )
        .unwrap();

        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["gjc".to_string()]),
            ..Default::default()
        };
        let local = super::generate_local_graph_report_local_only(options.clone())
            .await
            .unwrap();

        let pricing = pricing::PricingService::new(
            HashMap::from([(
                "claude-sonnet-4".to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.01),
                    output_cost_per_token: Some(0.02),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        let priced = super::generate_graph_with_loaded_pricing(
            options,
            Some(&pricing),
            GraphPricingMode::BestEffort,
        )
        .await
        .unwrap();

        let local_contract = local.contract();
        assert_eq!(local_contract.pricing_mode, GraphPricingMode::LocalOnly);
        assert_eq!(local_contract.cost_coverage, CostCoverage::Partial);
        assert_eq!(priced.contract().pricing_mode, GraphPricingMode::BestEffort);
        assert_eq!(priced.contract().cost_coverage, CostCoverage::Complete);

        let local_wire = serde_json::to_value(local.graph()).unwrap();
        assert!(local_wire["meta"].get("pricingMode").is_none());
        assert_eq!(local.contract().pricing_mode, GraphPricingMode::LocalOnly);

        let local_graph = local.graph();
        let priced_graph = priced.graph();
        assert_eq!(
            local_graph.summary.total_tokens,
            priced_graph.summary.total_tokens
        );
        assert_eq!(
            local_graph.summary.total_days,
            priced_graph.summary.total_days
        );
        assert_eq!(
            local_graph.summary.active_days,
            priced_graph.summary.active_days
        );
        assert_eq!(local_graph.summary.clients, priced_graph.summary.clients);
        assert_eq!(local_graph.summary.models, priced_graph.summary.models);
        assert!(priced_graph.summary.total_cost > local_graph.summary.total_cost);
        assert!(local_graph
            .contributions
            .iter()
            .flat_map(|day| day.clients.iter())
            .any(|client| (client.cost - 0.3).abs() < 1e-9));

        assert_eq!(
            local_graph.contributions.len(),
            priced_graph.contributions.len()
        );
        for (local_day, priced_day) in local_graph
            .contributions
            .iter()
            .zip(&priced_graph.contributions)
        {
            assert_eq!(local_day.date, priced_day.date);
            assert_eq!(local_day.totals.tokens, priced_day.totals.tokens);
            assert_eq!(local_day.totals.messages, priced_day.totals.messages);
            assert_eq!(local_day.token_breakdown, priced_day.token_breakdown);
            assert_eq!(local_day.turns_by_client, priced_day.turns_by_client);
            let local_topology: Vec<_> = local_day
                .clients
                .iter()
                .map(|client| {
                    (
                        client.client.as_str(),
                        client.model_id.as_str(),
                        client.provider_id.as_str(),
                        client.tokens.clone(),
                        client.messages,
                    )
                })
                .collect();
            let priced_topology: Vec<_> = priced_day
                .clients
                .iter()
                .map(|client| {
                    (
                        client.client.as_str(),
                        client.model_id.as_str(),
                        client.provider_id.as_str(),
                        client.tokens.clone(),
                        client.messages,
                    )
                })
                .collect();
            assert_eq!(local_topology, priced_topology);
        }
    }

    fn parse_claude_fixture(source_home: &Path) -> Vec<UnifiedMessage> {
        parse_all_messages_with_pricing_with_env_strategy(
            source_home.to_str().unwrap(),
            &["claude".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        )
    }

    fn write_gjc_fixture(source_home: &Path, model_id: &str, timestamp_ms: i64) {
        let gjc_dir = source_home.join(".gjc/agent/sessions");
        std::fs::create_dir_all(&gjc_dir).unwrap();
        std::fs::write(
            gjc_dir.join("test.jsonl"),
            format!(
                concat!(
                    "{{\"type\":\"session\",\"id\":\"gjc_ses_001\",\"cwd\":\"/work/pi\"}}\n",
                    "{{\"type\":\"message\",\"id\":\"m1\",\"message\":{{\"role\":\"assistant\",",
                    "\"model\":\"{model_id}\",\"provider\":\"anthropic\",\"timestamp\":{timestamp_ms},",
                    "\"usage\":{{\"input\":100,\"output\":50,\"cost\":{{\"total\":0.3}}}}}}}}\n",
                ),
                model_id = model_id,
                timestamp_ms = timestamp_ms,
            ),
        )
        .unwrap();
    }

    // Introduced by this change: an inverted/empty `[from_ms, until_ms)`
    // window is an answer (no messages), not a caller error. Verified by
    // hand-mutation: reintroducing `if until_ms <= from_ms { return
    // Err(...) }` at the top of `get_window_usage_inner` turns this from a
    // pass into a failure on the `.is_ok()` assertion below.
    #[tokio::test]
    #[serial_test::serial]
    async fn get_window_usage_returns_empty_not_error_for_inverted_range() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        write_gjc_fixture(source_home.path(), "claude-sonnet-4", 1767225601000);

        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["gjc".to_string()]),
            ..Default::default()
        };

        // from > until: no message's timestamp can ever satisfy the filter.
        let usage = get_window_usage(options, 1_700_000_060_000, 1_700_000_000_000)
            .await
            .expect("an inverted window must be Ok, not an error");
        assert!(usage.messages.is_empty());
    }

    // Regression pinned by this change: `40b73f5` routed every row's model id
    // through `canonical_model_id` so window rows carry the same identities
    // the aggregate reports do; the intermediate branch lost that (it used
    // `msg.model_id.clone()`). Verified by hand-mutation: swapping
    // `canonical_model_id(&msg.model_id)` for `msg.model_id.clone()` in
    // `get_window_usage_inner` turns this from a pass into a failure on the
    // `assert_eq!` below (raw id carries the `-20250101` date suffix that
    // canonicalization strips).
    #[tokio::test]
    #[serial_test::serial]
    async fn get_window_usage_canonicalizes_model_id() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let raw_model_id = "claude-sonnet-4-20250101";
        let timestamp_ms: i64 = 1767225601000;
        write_gjc_fixture(source_home.path(), raw_model_id, timestamp_ms);

        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["gjc".to_string()]),
            ..Default::default()
        };

        let usage = get_window_usage(options, timestamp_ms - 1, timestamp_ms + 1)
            .await
            .expect("window scan must succeed");
        assert_eq!(usage.messages.len(), 1);
        assert_eq!(usage.messages[0].model_id, canonical_model_id(raw_model_id));
        assert_ne!(
            usage.messages[0].model_id, raw_model_id,
            "the fixture's raw id must actually change under canonicalization, or this test proves nothing"
        );
    }

    async fn claude_window_output(context: &crate::ResolvedLocalSourceContext) -> i64 {
        let options = ReportOptions {
            clients: Some(vec!["claude".to_string()]),
            ..Default::default()
        };
        super::get_window_usage_with_source_context(
            context,
            options,
            1767225600000 - 1,
            1767225600000 + 1,
        )
        .await
        .expect("window scan must succeed")
        .messages
        .iter()
        .map(|message| message.output)
        .sum()
    }

    /// A captured context honors `excluded_scan_paths` through the window
    /// entry, in tokens: cold, and again after the control has put the excluded
    /// file in the shared source cache (a cached entry must not bring an
    /// unscanned file back).
    #[tokio::test]
    #[serial_test::serial]
    async fn get_window_usage_with_source_context_honors_excluded_scan_paths() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let home = source_home.path();
        write_claude_message(
            &home.join(".claude/projects/p"),
            "primary.jsonl",
            "m-primary",
            100,
        );
        let work = home.join("work-d");
        write_claude_message(&work.join("projects/p"), "d.jsonl", "m-d", 7_000);
        let capture = |excluded: Vec<PathBuf>| {
            crate::ResolvedLocalSourceContext::capture(
                Some(home.to_path_buf()),
                false,
                scanner::ScannerSettings {
                    extra_scan_paths: BTreeMap::from([(
                        "claude".to_string(),
                        vec![work.join("projects")],
                    )]),
                    excluded_scan_paths: BTreeMap::from([("claude".to_string(), excluded)]),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        let excluded = capture(vec![work.clone()]);
        let control = capture(Vec::new());

        assert_eq!(claude_window_output(&excluded).await, 100, "cold");
        assert_eq!(
            claude_window_output(&control).await,
            7_100,
            "control sees both"
        );
        assert_eq!(claude_window_output(&excluded).await, 100, "warm cache");
    }

    fn collect_streamed_claude_fixture(source_home: &Path) -> Vec<UnifiedMessage> {
        let mut messages = Vec::new();
        scan_messages_streaming(
            source_home.to_str().unwrap(),
            &["claude".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| messages.push(message.clone()),
        );
        messages
    }

    /// Claude Code rewrites a session transcript in place on resume/compact:
    /// the file keeps its path and session id but loses already-written
    /// assistant turns. Because the source cache tracks live file content, a
    /// rescan after such a rewrite used to drop those turns from history for
    /// good.
    ///
    /// This is also the hermetic end-to-end reproduction of the defect: every
    /// expected total below is hand-derived from the fixture bytes written
    /// here, never read back from the code under test.
    #[test]
    #[serial_test::serial]
    fn test_claude_in_place_rewrite_preserves_previously_seen_messages() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");

        let turn_one = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":7,"cache_creation_input_tokens":3}}}"#;
        let turn_two = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_002","message":{"id":"msg_002","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60,"cache_read_input_tokens":11,"cache_creation_input_tokens":5}}}"#;
        let turn_three = r#"{"type":"assistant","timestamp":"2024-12-01T10:10:00.000Z","requestId":"req_003","message":{"id":"msg_003","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70,"cache_read_input_tokens":13,"cache_creation_input_tokens":17}}}"#;

        std::fs::write(
            &transcript,
            format!("{turn_one}\n{turn_two}\n{turn_three}\n"),
        )
        .unwrap();

        let before = parse_claude_fixture(source_home.path());
        assert_eq!(before.len(), 3, "cold scan must see all three turns");
        // Hand-derived from the fixture literals above, not from the parse output.
        let expected_output = 50 + 60 + 70;
        let expected_cache_read = 7 + 11 + 13;
        let expected_cache_write = 3 + 5 + 17;
        assert_eq!(
            before.iter().map(|m| m.tokens.output).sum::<i64>(),
            expected_output
        );

        // The rewrite: same path, same session, two assistant turns gone.
        std::fs::write(&transcript, format!("{turn_three}\n")).unwrap();

        let after = parse_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            3,
            "an in-place rewrite must not retire messages the cache already observed"
        );
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            expected_output
        );
        assert_eq!(
            after.iter().map(|m| m.tokens.cache_read).sum::<i64>(),
            expected_cache_read
        );
        assert_eq!(
            after.iter().map(|m| m.tokens.cache_write).sum::<i64>(),
            expected_cache_write
        );

        // Retention has to survive its own round trip through the cache: the
        // rewritten entry is what the NEXT scan reads back, so a union that is
        // computed but not persisted drifts one run later.
        let third = parse_claude_fixture(source_home.path());
        assert_eq!(
            third.len(),
            3,
            "the retained turns must be written back to the cache, not just returned once"
        );
        assert_eq!(
            third.iter().map(|m| m.tokens.output).sum::<i64>(),
            expected_output
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_context_token_tracks_claude_retained_store_changes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");
        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let target = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_target","message":{"id":"msg_target","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let original = format!("{filler}\n{target}\n");
        let compacted = format!("{filler}\n");
        std::fs::write(&transcript, &original).unwrap();

        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let local_options = || LocalParseOptions {
            home_dir: None,
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let report_options = || ReportOptions {
            clients: Some(vec!["claude".to_string()]),
            ..Default::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let token_before =
            local_source_change_token_with_source_context(&context, &local_options()).unwrap();
        let seeded = runtime
            .block_on(get_model_report_with_source_context(
                &context,
                report_options(),
            ))
            .unwrap();
        assert_eq!(seeded.total_messages, 2);
        assert_eq!(seeded.total_output, 60);
        assert_eq!(
            token_before,
            local_source_change_token_with_source_context(&context, &local_options()).unwrap(),
            "creating a live-only cache entry must not change the context token"
        );

        std::fs::write(&transcript, &compacted).unwrap();
        let retained = runtime
            .block_on(get_model_report_with_source_context(
                &context,
                report_options(),
            ))
            .unwrap();
        assert_eq!(retained.total_messages, 2);
        assert_eq!(retained.total_output, 60);
        let retained_token =
            local_source_change_token_with_source_context(&context, &local_options()).unwrap();
        assert_ne!(retained_token, token_before);
        assert_eq!(
            retained_token,
            local_source_change_token_with_source_context(&context, &local_options()).unwrap(),
            "unchanged retained state must keep both the token and report stable"
        );

        let cache_root = context.usage_dir();
        let source_metadata = std::fs::metadata(&transcript).unwrap();
        let retained_before_replacement =
            local_source_change_token_with_source_context(&context, &local_options()).unwrap();

        let mut cache = message_cache::SourceMessageCache::load_from_root(Some(cache_root));
        let mut replacement = cache
            .entries_for_test()
            .into_iter()
            .find(|entry| entry.retained_keys.contains("msg_target:req_target"))
            .expect("the compacted report must persist the retained target")
            .clone();
        replacement
            .messages
            .iter_mut()
            .find(|message| message.dedup_key.as_deref() == Some("msg_target:req_target"))
            .expect("the retained target message must be present")
            .tokens
            .output = 999;
        cache.insert(replacement);
        cache.save_if_dirty();

        let replacement_token =
            local_source_change_token_with_source_context(&context, &local_options()).unwrap();
        assert_ne!(replacement_token, retained_before_replacement);
        let changed = runtime
            .block_on(get_model_report_with_source_context(
                &context,
                report_options(),
            ))
            .unwrap();
        assert_eq!(changed.total_messages, 2);
        assert_eq!(changed.total_output, 1009);
        let source_metadata_after = std::fs::metadata(&transcript).unwrap();
        assert_eq!(source_metadata.len(), source_metadata_after.len());
        assert_eq!(
            source_metadata.modified().unwrap(),
            source_metadata_after.modified().unwrap()
        );
    }

    /// Retention is only sound because a retained message still collapses
    /// against a live copy of itself somewhere else. Claude Code forks a
    /// session into a new transcript that replays earlier turns, so the same
    /// `messageId:requestId` legitimately appears in two files at once.
    ///
    /// The transcripts are named so the retaining file sorts first: the lane
    /// walks paths in lexical order and keeps the first copy of a key, so this
    /// is the ordering where a retained copy wins over the live one.
    #[test]
    #[serial_test::serial]
    fn test_claude_retained_message_collapses_against_a_forked_transcript() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let turn_one = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_two = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_002","message":{"id":"msg_002","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;
        let turn_three = r#"{"type":"assistant","timestamp":"2024-12-01T10:10:00.000Z","requestId":"req_003","message":{"id":"msg_003","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70}}}"#;

        std::fs::write(&original, format!("{turn_one}\n{turn_two}\n{turn_three}\n")).unwrap();
        let before = parse_claude_fixture(source_home.path());
        assert_eq!(before.len(), 3);

        // The compaction keeps turn one. Turn two is replayed into the fork,
        // so it exists both retained and live. Turn three exists only as a
        // retained copy.
        std::fs::write(&original, format!("{turn_one}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{turn_two}\n")).unwrap();

        let after = parse_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            3,
            "retention must keep turn three and must not double count turn two"
        );
        let keys: HashSet<String> = after
            .iter()
            .filter_map(|message| message.dedup_key.clone())
            .collect();
        assert_eq!(keys.len(), 3, "every surviving message must be distinct");
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            50 + 60 + 70,
            "turn two contributes its 60 once, not twice"
        );
        assert_eq!(
            after.iter().map(|m| m.tokens.input).sum::<i64>(),
            100 + 200 + 300
        );
    }

    /// A retained message is a partial the parser cached mid-stream (see
    /// `merge_claude_duplicate`'s per-field max in `sessions/claudecode.rs`),
    /// and compaction can drop that turn from the original file before the
    /// stream ever completed. A forked/resumed transcript that replays the
    /// same `messageId:requestId` after it finished must win: the live parse
    /// of the fork carries the completed usage, and retention must not let
    /// the earlier, lower partial from the original file's cache shadow it.
    ///
    /// The fork is named to sort after the original, i.e. the ordering where
    /// naive first-write-wins dedup keeps the retained (wrong) copy.
    #[test]
    #[serial_test::serial]
    fn test_claude_live_copy_outranks_retained_partial_when_fork_sorts_after() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = parse_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2, "cold scan sees the filler and the partial");
        // Hand-derived from the fixture literals above.
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        // Compaction drops the shared turn from the original, leaving only the
        // filler; a fork sorting after the original replays the shared turn
        // to completion.
        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{completed}\n")).unwrap();

        let after = parse_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            2,
            "the shared key must not double count across original and fork"
        );
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "the live completed copy from the fork must win over the retained partial"
        );
    }

    /// Same defect, opposite file order: the fork is named to sort *before*
    /// the original, so a test that only exercised one ordering could pass by
    /// accident of iteration order rather than because retained messages are
    /// actually deferred behind every live message.
    #[test]
    #[serial_test::serial]
    fn test_claude_live_copy_outranks_retained_partial_when_fork_sorts_before() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("zzz-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = parse_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("aaa-fork.jsonl"), format!("{completed}\n")).unwrap();

        let after = parse_claude_fixture(source_home.path());
        assert_eq!(after.len(), 2);
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "file iteration order must not change which copy wins"
        );
    }

    /// A Claude tool-result key embeds the session id, which is the
    /// transcript's file stem. A retained tool result therefore could never
    /// collapse against the same tool result replayed under a fork's filename
    /// — both would count — so retention has to leave those records behind
    /// even though it means a compaction still retires their input tokens.
    #[test]
    #[serial_test::serial]
    fn test_claude_path_scoped_tool_result_is_not_retained_across_a_rewrite() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");

        let assistant_turn = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let tool_result = r#"{"type":"user","timestamp":"2024-12-01T10:01:00.000Z","message":{"model":"claude-3-5-sonnet","content":[{"type":"tool_result","tool_use_id":"toolu_1","tool_output":{"input_tokens":40,"output":"result"}}]}}"#;

        std::fs::write(&transcript, format!("{assistant_turn}\n{tool_result}\n")).unwrap();
        let before = parse_claude_fixture(source_home.path());
        assert_eq!(
            before.len(),
            2,
            "cold scan sees the turn and the tool result"
        );
        assert_eq!(before.iter().map(|m| m.tokens.input).sum::<i64>(), 100 + 40);

        // The rewrite drops both records; only the assistant turn is
        // re-added, so the tool result is a candidate for retention.
        std::fs::write(&transcript, format!("{assistant_turn}\n")).unwrap();

        let after = parse_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            1,
            "a path-scoped key must not be carried across the rewrite"
        );
        assert_eq!(after.iter().map(|m| m.tokens.input).sum::<i64>(), 100);
    }

    /// The streaming lane is a local integration seam absent from upstream:
    /// our fork's `scan_messages_streaming` also has to honor retention, not
    /// just the materialized path exercised above.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_lane_preserves_previously_seen_messages_across_a_rewrite() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");

        let turn_one = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_two = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_002","message":{"id":"msg_002","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

        std::fs::write(&transcript, format!("{turn_one}\n{turn_two}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2, "cold streaming scan sees both turns");

        std::fs::write(&transcript, format!("{turn_two}\n")).unwrap();
        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            2,
            "the streaming lane must retain turn one across the rewrite too"
        );
        assert_eq!(after.iter().map(|m| m.tokens.output).sum::<i64>(), 50 + 60);
    }

    /// A warm cache hit (unchanged fingerprint) must never run the retention
    /// machinery at all — it used to clone the entire cached vector before
    /// even checking the fingerprint, discarding the clone on every warm hit.
    /// `retain_observed_messages` is only reachable from the fingerprint-
    /// changed miss branch, so its call count is a faithful proxy for "the
    /// discarded clone work ran" without instrumenting the clone directly.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_warm_cache_hit_skips_retention_clone_work() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");

        let turn_one = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_two = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_002","message":{"id":"msg_002","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

        std::fs::write(&transcript, format!("{turn_one}\n{turn_two}\n")).unwrap();
        collect_streamed_claude_fixture(source_home.path());

        // The rewrite drops turn one, forcing a fingerprint-changed miss that
        // retains it — this is the only kind of scan allowed to run
        // retention, so it establishes the "after a real miss" baseline.
        std::fs::write(&transcript, format!("{turn_two}\n")).unwrap();
        let after_rewrite = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            after_rewrite.len(),
            2,
            "turn one must be retained after the rewrite"
        );
        let calls_after_miss =
            RETAIN_OBSERVED_MESSAGES_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            calls_after_miss > 0,
            "the miss branch must have run retention at least once"
        );

        // The file is now unchanged since the last scan: this must be a warm
        // cache hit that never touches retention (and therefore never clones
        // the cached vector the finding described).
        let warm = collect_streamed_claude_fixture(source_home.path());
        let calls_after_warm =
            RETAIN_OBSERVED_MESSAGES_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            calls_after_warm, calls_after_miss,
            "a warm cache hit on an unchanged file must not run the miss-branch retention/clone work"
        );
        assert_eq!(
            warm.len(),
            2,
            "the warm hit must still return the retained turn"
        );
        assert_eq!(warm.iter().map(|m| m.tokens.output).sum::<i64>(), 50 + 60);
    }

    /// Finding 2: a retained message must not keep the client/provider/
    /// workspace metadata of the parse that first cached it. A `cc-mirror`
    /// variant edit changes the Claude fingerprint (variant.json rides along
    /// as a related file — see `check_claude_code_path_with_home_mode`)
    /// without touching the transcript body, so a rescan re-derives live
    /// messages' metadata from the new variant file while a message carried
    /// forward by retention must be rebuilt to match, not left on the stale
    /// values from the scan that cached it.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_retained_message_refreshes_stale_cc_mirror_metadata() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let variant_dir = source_home.path().join(".cc-mirror").join("zai-worker");
        let config_dir = variant_dir.join("config");
        let project_dir = config_dir.join("projects").join("-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let variant_path = variant_dir.join("variant.json");
        std::fs::write(
            &variant_path,
            serde_json::json!({
                "name": "zai-worker",
                "provider": "zai",
                "configDir": config_dir,
            })
            .to_string(),
        )
        .unwrap();
        let transcript = project_dir.join("session.jsonl");

        let turn_a = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_b = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_b","message":{"id":"msg_b","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert!(
            before
                .iter()
                .all(|m| m.client == "cc-mirror/zai-worker" && m.provider_id == "zai"),
            "cold scan must attribute both turns to the original variant"
        );

        // Turn A drops out of the live file, so it becomes a retained
        // message cached under the original variant's client/provider.
        std::fs::write(&transcript, format!("{turn_b}\n")).unwrap();
        let after_drop = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after_drop.len(), 2, "turn A must be retained");

        // The variant is renamed and rehomed to a different provider. The
        // transcript itself is untouched, but variant.json riding along in
        // the fingerprint means this alone forces a fresh scan.
        std::fs::write(
            &variant_path,
            serde_json::json!({
                "name": "zai-worker-mk2",
                "provider": "moonshot",
                "configDir": config_dir,
            })
            .to_string(),
        )
        .unwrap();

        let after_rename = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            after_rename.len(),
            2,
            "turn A must still be retained after the variant edit"
        );
        let retained = after_rename
            .iter()
            .find(|m| m.dedup_key.as_deref() == Some("msg_a:req_a"))
            .expect("turn A must survive as a retained message");
        let live = after_rename
            .iter()
            .find(|m| m.dedup_key.as_deref() == Some("msg_b:req_b"))
            .expect("turn B must be present as a live message");
        assert_eq!(
            live.client, "cc-mirror/zai-worker-mk2",
            "sanity check: the live message picks up the renamed variant"
        );
        assert_eq!(live.provider_id, "moonshotai");
        assert_eq!(
            retained.client, live.client,
            "the retained turn must be rebuilt onto the new variant's client id, not kept on the stale one"
        );
        assert_eq!(
            retained.provider_id, live.provider_id,
            "the retained turn must be rebuilt onto the new variant's provider, not kept on the stale one"
        );
        assert_eq!(retained.tokens.output, 50, "token counts stay intrinsic");
    }

    /// `refresh_retained_message_context` must rebuild `provider_id` from the
    /// current parse's evidence, not merge with the retained message's own
    /// stale value. The regression this guards: seeding the merge confidence
    /// from `stored_claude_provider_confidence(&message.provider_id)` reads
    /// the *cached* provider back, so a stale non-Anthropic provider
    /// (`CLAUDE_PROVIDER_INFERRED_CONFIDENCE`) outranks the live parse's
    /// Anthropic candidate (`CLAUDE_PROVIDER_DEFAULT_CONFIDENCE`, strictly
    /// lower) and survives the refresh. The sibling test above only exercises
    /// a non-Anthropic-to-non-Anthropic edit (zai -> moonshot), which already
    /// worked under the old seeding because both candidates use
    /// `CLAUDE_PROVIDER_INFERRED_CONFIDENCE`.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_retained_provider_id_matches_live_sibling_after_variant_turns_anthropic(
    ) {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let variant_dir = source_home.path().join(".cc-mirror").join("zai-worker");
        let config_dir = variant_dir.join("config");
        let project_dir = config_dir.join("projects").join("-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let variant_path = variant_dir.join("variant.json");
        std::fs::write(
            &variant_path,
            serde_json::json!({
                "name": "zai-worker",
                "provider": "zai",
                "configDir": config_dir,
            })
            .to_string(),
        )
        .unwrap();
        let transcript = project_dir.join("session.jsonl");

        let turn_a = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_b = r#"{"type":"assistant","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_b","message":{"id":"msg_b","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert!(
            before.iter().all(|m| m.provider_id == "zai"),
            "cold scan must attribute both turns to the original variant's provider"
        );

        // Turn A drops out of the live file, so it becomes a retained
        // message cached under the original variant's provider ("zai",
        // CLAUDE_PROVIDER_INFERRED_CONFIDENCE).
        std::fs::write(&transcript, format!("{turn_b}\n")).unwrap();
        let after_drop = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after_drop.len(), 2, "turn A must be retained");

        // The variant is rehomed to Anthropic itself. The candidate this
        // resolves to (CLAUDE_PROVIDER_DEFAULT_CONFIDENCE) is strictly LOWER
        // than the stale "zai" reading, so a confidence seeded from the
        // retained message's own value would reject it.
        std::fs::write(
            &variant_path,
            serde_json::json!({
                "name": "zai-worker",
                "provider": "anthropic",
                "configDir": config_dir,
            })
            .to_string(),
        )
        .unwrap();

        let after_rename = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            after_rename.len(),
            2,
            "turn A must still be retained after the variant edit"
        );
        let retained = after_rename
            .iter()
            .find(|m| m.dedup_key.as_deref() == Some("msg_a:req_a"))
            .expect("turn A must survive as a retained message");
        let live = after_rename
            .iter()
            .find(|m| m.dedup_key.as_deref() == Some("msg_b:req_b"))
            .expect("turn B must be present as a live message");
        assert_eq!(
            live.provider_id, "anthropic",
            "sanity check: the live message picks up the variant's new provider"
        );
        assert_eq!(
            retained.provider_id, live.provider_id,
            "the retained turn must adopt the live parse's provider even though it is a \
             lower-confidence candidate than the stale cached value"
        );
    }

    /// `refresh_retained_message_context` must not clear a retained message's
    /// `agent` when the live parse produced no sibling to read it from. The
    /// regression this guards: unconditionally assigning
    /// `live_agent.map(str::to_string)` erases the cached subagent name once
    /// a compacting rewrite removes every live assistant row from a
    /// sidechain transcript, moving retained-only sidechain usage into the
    /// main/unknown bucket until the cache is rebuilt from a transcript with
    /// live rows again.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_retained_sidechain_agent_survives_a_rewrite_with_no_live_siblings() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let subagents_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject")
            .join("parent-uuid-100")
            .join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let transcript = subagents_dir.join("agent-xyz100.jsonl");
        std::fs::write(
            subagents_dir.join("agent-xyz100.meta.json"),
            r#"{"agentType":"explore"}"#,
        )
        .unwrap();

        let turn_a = r#"{"type":"assistant","isSidechain":true,"sessionId":"parent-uuid-100","agentId":"xyz100","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let turn_b = r#"{"type":"assistant","isSidechain":true,"sessionId":"parent-uuid-100","agentId":"xyz100","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_b","message":{"id":"msg_b","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert!(
            before.iter().all(|m| m.agent.as_deref() == Some("Explore")),
            "cold scan must resolve both turns to the meta.json subagent name"
        );

        // A compacting rewrite drops every assistant usage row from the
        // sidechain file, leaving only a sidechain user row. The live parse
        // of this file now produces zero messages, so retention has no live
        // sibling to read `agent` from for either retained turn.
        let user_row = r#"{"type":"user","isSidechain":true,"sessionId":"parent-uuid-100","agentId":"xyz100","timestamp":"2024-12-01T10:10:00.000Z","message":{"content":"continuing"}}"#;
        std::fs::write(&transcript, format!("{user_row}\n")).unwrap();

        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            after.len(),
            2,
            "both turns must be retained even with no live sibling in the same scan"
        );
        assert!(
            after.iter().all(|m| m.agent.as_deref() == Some("Explore")),
            "the retained turns must keep their cached agent when there is no live \
             evidence to replace it, not be cleared to the main/unknown bucket"
        );
    }

    /// The sibling `.meta.json` is part of the transcript's fingerprint, so
    /// editing it reparses the entry. With every live assistant row compacted
    /// away there is no sibling to take an agent from, but that is not the same
    /// as there being no current metadata -- the sidecar still names the agent,
    /// and a retained-only turn must move with it rather than staying on the
    /// name it was cached under.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_retained_sidechain_agent_follows_a_meta_json_rename() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let subagents_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject")
            .join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let transcript = subagents_dir.join("agent-xyz200.jsonl");
        let meta = subagents_dir.join("agent-xyz200.meta.json");
        std::fs::write(&meta, r#"{"agentType":"explore"}"#).unwrap();

        let turn = r#"{"type":"assistant","isSidechain":true,"sessionId":"parent-uuid-200","agentId":"xyz200","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_a","message":{"id":"msg_a","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        std::fs::write(&transcript, format!("{turn}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 1, "cold scan sees the sidechain turn");
        assert_eq!(
            before[0].agent.as_deref(),
            Some("Explore"),
            "the cold parse takes the agent from the sidecar"
        );

        // Compaction empties the transcript of live assistant rows, and the
        // sidecar is renamed. The rename alone changes the fingerprint.
        std::fs::write(&transcript, b"").unwrap();
        std::fs::write(&meta, r#"{"agentType":"plan"}"#).unwrap();

        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after.len(), 1, "the retained turn must survive");
        assert_eq!(
            after[0].agent.as_deref(),
            Some("Plan"),
            "a retained-only turn must follow the sidecar it is fingerprinted against"
        );
    }

    /// Streaming-lane counterpart of
    /// `test_claude_live_copy_outranks_retained_partial_when_fork_sorts_after`:
    /// the streaming lane has its own dedup gate (`claude_seen` /
    /// `dedup_gate_passes`) and its own file loop, so it needs its own
    /// regression rather than inheriting the materialized lane's fix.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_live_copy_outranks_retained_partial_when_fork_sorts_after() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{completed}\n")).unwrap();

        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after.len(), 2);
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "the streaming lane's live completed copy must win over its retained partial"
        );
    }

    /// Same defect, opposite file order — see
    /// `test_claude_live_copy_outranks_retained_partial_when_fork_sorts_before`
    /// for why both orderings are asserted.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_live_copy_outranks_retained_partial_when_fork_sorts_before() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("zzz-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.len(), 2);
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("aaa-fork.jsonl"), format!("{completed}\n")).unwrap();

        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after.len(), 2);
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "file iteration order must not change which copy wins"
        );
    }

    /// The rewrite-and-fork scan above already resolves correctly, and writes
    /// the resolved messages back to the cache — but a fresh parse's
    /// `retained_keys` used to live only in `CachedParseOutcome`, never in the
    /// persisted `CachedSourceEntry`. The very next scan then reads the
    /// original file's entry back with no provenance at all, treats the
    /// carried-forward partial as if it were live output of this scan, and it
    /// reclaims the key ahead of the fork's completed copy — the undercount
    /// this whole change exists to fix returns within one more scan even
    /// though nothing on disk moved. This is the test that fails without
    /// `CachedSourceEntry::retained_keys`.
    #[test]
    #[serial_test::serial]
    fn test_claude_third_unchanged_scan_still_prefers_completed_over_retained_partial() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = parse_claude_fixture(source_home.path());
        // Hand-derived from the fixture literals above.
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        // The rewrite-and-fork scan: compaction drops the shared turn from
        // the original, and a fork sorting after it replays that turn to
        // completion.
        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{completed}\n")).unwrap();
        let after = parse_claude_fixture(source_home.path());
        assert_eq!(
            after.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "the rewrite scan must already prefer the fork's completed replay"
        );

        // Nothing on disk changes between this scan and the last one: every
        // file's fingerprint is unchanged, so this is a cache hit throughout.
        let stable = parse_claude_fixture(source_home.path());
        assert_eq!(
            stable.len(),
            2,
            "a cache-hit rescan must not resurrect the retained partial as a second message"
        );
        assert_eq!(
            stable.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "a cache-hit rescan must not let the formerly-retained partial reclaim the key"
        );
    }

    /// Streaming-lane counterpart of
    /// `test_claude_third_unchanged_scan_still_prefers_completed_over_retained_partial`:
    /// the streaming lane reconstructs retained status from the same
    /// persisted `CachedSourceEntry::retained_keys` on its own cache-hit path,
    /// and needs its own regression rather than inheriting the materialized
    /// lane's fix.
    #[test]
    #[serial_test::serial]
    fn test_claude_streaming_third_unchanged_scan_still_prefers_completed_over_retained_partial() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let before = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(before.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 50);

        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{completed}\n")).unwrap();
        let after = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(after.iter().map(|m| m.tokens.output).sum::<i64>(), 10 + 999);

        let stable = collect_streamed_claude_fixture(source_home.path());
        assert_eq!(
            stable.len(),
            2,
            "a cache-hit rescan must not resurrect the retained partial as a second message"
        );
        assert_eq!(
            stable.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 999,
            "a cache-hit rescan must not let the formerly-retained partial reclaim the key"
        );
    }

    /// The materialized lane and the streaming lane use different mechanisms
    /// for the live-outranks-retained invariant (two-pass admission driven by
    /// an in-memory `CachedParseOutcome` vs. a deferred-message buffer driven
    /// by `CachedSourceEntry::retained_keys` on a cache hit), so a fix to one
    /// must not silently diverge from the other. This runs the materialized
    /// lane first to populate the on-disk cache with `retained_keys`
    /// provenance, then has the streaming lane read that same persisted
    /// provenance back and checks it resolves to the same totals — not just
    /// that each lane agrees with a parse it did itself.
    #[test]
    #[serial_test::serial]
    fn test_claude_materialized_and_streaming_lanes_agree_on_one_cache_after_rewrite() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let partial = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let completed = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_shared","message":{"id":"msg_shared","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":999}}}"#;

        std::fs::write(&original, format!("{filler}\n{partial}\n")).unwrap();
        let _ = parse_claude_fixture(source_home.path());

        std::fs::write(&original, format!("{filler}\n")).unwrap();
        std::fs::write(claude_dir.join("zzz-fork.jsonl"), format!("{completed}\n")).unwrap();

        let materialized_total: i64 = parse_claude_fixture(source_home.path())
            .iter()
            .map(|m| m.tokens.output)
            .sum();
        assert_eq!(
            materialized_total,
            10 + 999,
            "materialized lane must resolve to the fork's completed replay"
        );

        let streaming_total: i64 = collect_streamed_claude_fixture(source_home.path())
            .iter()
            .map(|m| m.tokens.output)
            .sum();
        assert_eq!(
            streaming_total, materialized_total,
            "the streaming lane must agree with the materialized lane against the same persisted cache"
        );
    }

    /// `parse_local_clients` used to parse Claude files directly
    /// (`parse_claude_file_with_cache_and_home`), bypassing the
    /// `SourceMessageCache`-backed retention the report lanes use. A pure
    /// compaction — a turn dropped from the transcript with no fork
    /// replaying it elsewhere — is only recoverable through that retention:
    /// there is no live copy anywhere on disk for a direct parse to find.
    /// This asserts `parse_local_clients`'s totals for the compacted tree
    /// agree with `get_model_report`'s for the same tree, which requires
    /// the dropped turn to survive in both.
    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_agrees_with_model_report_after_a_compacting_rewrite() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let original = claude_dir.join("aaa-original.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let dropped = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_dropped","message":{"id":"msg_dropped","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;

        let local_options = || LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let report_options = || ReportOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            group_by: GroupBy::ClientModel,
            scanner_settings: scanner::ScannerSettings::default(),
        };

        // Populate the persistent cache with the turn compaction is about to
        // drop.
        std::fs::write(&original, format!("{filler}\n{dropped}\n")).unwrap();
        let before = parse_local_clients(local_options()).unwrap();
        assert_eq!(
            before.messages.iter().map(|m| m.output).sum::<i64>(),
            10 + 50,
            "hand-derived from the fixture literals above"
        );

        // Compaction drops the turn with no fork replaying it elsewhere —
        // only retention can recover it.
        std::fs::write(&original, format!("{filler}\n")).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let model_report = runtime
            .block_on(get_model_report(report_options()))
            .unwrap();
        let local_after = parse_local_clients(local_options()).unwrap();

        assert_eq!(
            local_after.messages.iter().map(|m| m.output).sum::<i64>(),
            10 + 50,
            "parse_local_clients must retain the compacted turn like the report lanes do"
        );
        assert_eq!(
            local_after.messages.iter().map(|m| m.output).sum::<i64>(),
            model_report.total_output,
            "parse_local_clients and get_model_report must agree on the same source tree"
        );
    }

    /// A bump has to reach a transcript that no longer changes without
    /// discarding the turns only the cache holds (RET-CLAUDE-001). Both halves
    /// are asserted; the retained one alone would pass an implementation that
    /// dropped the live side.
    #[test]
    #[serial_test::serial]
    fn test_a_parser_version_bump_rebuilds_live_messages_and_keeps_retained_ones() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("session.jsonl");

        let live = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_live","message":{"id":"msg_live","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let dropped = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_dropped","message":{"id":"msg_dropped","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;

        let local_options = || LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let total_output = || {
            parse_local_clients(local_options())
                .unwrap()
                .messages
                .iter()
                .map(|m| m.output)
                .sum::<i64>()
        };

        std::fs::write(&transcript, format!("{live}\n{dropped}\n")).unwrap();
        assert_eq!(total_output(), 10 + 50);

        // From here the cache is the only copy of the second turn.
        std::fs::write(&transcript, format!("{live}\n")).unwrap();
        assert_eq!(total_output(), 10 + 50, "retention keeps the dropped turn");

        // Corrupt the live half in the cache: a hit replays 999, a re-parse
        // reads 10 off the transcript. That gap is what the last assertion
        // measures.
        let identity = message_cache::CacheIdentity::for_client(ClientId::Claude);
        let mut cache = message_cache::SourceMessageCache::load();
        assert_eq!(cache.entries_for_test().len(), 1);
        let mut entry = cache.entries_for_test().into_iter().next().unwrap().clone();
        for message in entry.messages.iter_mut() {
            if message.dedup_key.as_deref() == Some("msg_live:req_live") {
                message.tokens.output = 999;
            }
        }
        cache.insert(entry);
        cache.save_if_dirty();
        assert_eq!(
            total_output(),
            999 + 50,
            "an unchanged transcript is served from cache, corruption and all"
        );

        // What an older build left behind.
        message_cache::set_parser_version_for_test(identity, 2);
        {
            // Addressed through `entries` rather than by path: the scanner's
            // spelling of a path is its own, and on Windows it is not the one
            // this test built.
            let reloaded = message_cache::SourceMessageCache::load();
            let kept = reloaded
                .entries_for_test()
                .into_iter()
                .next()
                .expect("the downgraded entry is kept, not dropped");
            assert_eq!(kept.messages.len(), 2, "kept whole, for retention to read");
        }

        assert_eq!(
            total_output(),
            10 + 50,
            "the live turn is re-parsed from source and the retained one survives"
        );
    }

    /// The migrated entry has to outlive a scan that never reaches its source:
    /// the load queues the shard for rewrite either way, and the history is
    /// gone if the rewrite does not carry it. `modified_after` prunes the file
    /// before the parse, so nothing re-parses it and nothing marks it dirty.
    #[test]
    #[serial_test::serial]
    fn test_a_scan_that_skips_the_source_does_not_erase_its_migrated_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("session.jsonl");

        let live = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_live","message":{"id":"msg_live","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let dropped = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_dropped","message":{"id":"msg_dropped","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;

        let local_options = |modified_after| LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after,
        };
        let total_output = |modified_after| {
            parse_local_clients(local_options(modified_after))
                .unwrap()
                .messages
                .iter()
                .map(|m| m.output)
                .sum::<i64>()
        };

        std::fs::write(&transcript, format!("{live}\n{dropped}\n")).unwrap();
        assert_eq!(total_output(None), 10 + 50);
        std::fs::write(&transcript, format!("{live}\n")).unwrap();
        assert_eq!(total_output(None), 10 + 50);

        let identity = message_cache::CacheIdentity::for_client(ClientId::Claude);
        message_cache::set_parser_version_for_test(identity, 2);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        assert_eq!(
            total_output(Some(future_ms)),
            0,
            "sanity: the threshold really does prune the transcript"
        );

        assert_eq!(
            total_output(None),
            10 + 50,
            "the skipped scan must not have taken the history with it"
        );
    }

    /// `parse_local_clients` used to route Claude through
    /// `parse_all_messages_with_pricing_with_env_strategy`, which performed
    /// its own scan and knew nothing about `modified_after` — the pruned
    /// file list this function's own scan already computed was discarded, so
    /// a Claude transcript the caller meant to exclude by mtime was parsed
    /// and counted anyway, while every other file-backed client still
    /// honored the option. This asserts a future `modified_after` threshold
    /// excludes a Claude transcript exactly like it does for a client that
    /// never went through the retention-aware lane.
    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_honors_modified_after_for_claude() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("session.jsonl");
        let content = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_excluded","message":{"id":"msg_excluded","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        std::fs::write(&transcript, content).unwrap();

        let local_options = |modified_after| LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after,
        };

        let included = parse_local_clients(local_options(None)).unwrap();
        assert_eq!(
            included.messages.len(),
            1,
            "sanity: the transcript parses without a threshold"
        );

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let pruned = parse_local_clients(local_options(Some(future_ms))).unwrap();
        assert_eq!(
            pruned.messages.len(),
            0,
            "modified_after must exclude a Claude transcript older than the threshold, matching \
             how other file-backed clients behave under the same option"
        );
        assert_eq!(pruned.counts.get(ClientId::Claude), 0);
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_with_source_context_honors_modified_after_for_claude() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("session.jsonl"),
            r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_context","message":{"id":"msg_context","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
        )
        .unwrap();

        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let local_options = |modified_after| LocalParseOptions {
            home_dir: None,
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after,
        };

        let included =
            parse_local_clients_with_source_context(&context, local_options(None)).unwrap();
        assert_eq!(included.messages.len(), 1);
        assert_eq!(included.counts.get(ClientId::Claude), 1);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let pruned =
            parse_local_clients_with_source_context(&context, local_options(Some(future_ms)))
                .unwrap();
        assert!(pruned.messages.is_empty());
        assert_eq!(pruned.counts.get(ClientId::Claude), 0);
    }

    /// A deferred retained message's `date` is whatever was persisted the
    /// last time it was cached — potentially under a different local
    /// timezone or before a leap into the next day. The materialized lane
    /// refreshes `date` from `timestamp` for every message it returns,
    /// retained or not (see `apply_pricing_to_messages` /
    /// `cached_messages`), but the streaming lane's deferred-retained sink
    /// only used to reprice, not refresh. A stale `date` is invisible to a
    /// raw count/sum comparison — it only surfaces once something filters on
    /// `date`, exactly like `get_hourly_report`'s `since`/`until`/year
    /// filtering does in production. This test tampers a persisted retained
    /// message's `date` directly (standing in for "cached under a different
    /// timezone") and asserts both lanes agree on the *filtered* output,
    /// which requires the streaming lane to have recomputed `date` from the
    /// message's unchanged `timestamp` before the filter ever sees it.
    #[test]
    #[serial_test::serial]
    fn test_claude_materialized_and_streaming_lanes_agree_on_deferred_retained_date_after_filtering(
    ) {
        use chrono::TimeZone;

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let claude_dir = source_home
            .path()
            .join(".claude")
            .join("projects")
            .join("myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("conversation.jsonl");

        let filler = r#"{"type":"assistant","timestamp":"2024-12-01T09:00:00.000Z","requestId":"req_filler","message":{"id":"msg_filler","model":"claude-3-5-sonnet","usage":{"input_tokens":9,"output_tokens":10}}}"#;
        let target = r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_target","message":{"id":"msg_target","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
        let target_dedup_key = "msg_target:req_target";
        let target_timestamp_ms = chrono::DateTime::parse_from_rfc3339("2024-12-01T10:00:00.000Z")
            .unwrap()
            .timestamp_millis();
        // Same formula `UnifiedMessage::refresh_derived_fields` uses, computed
        // independently here rather than read back from the parse under test.
        let expected_date = chrono::Local
            .timestamp_millis_opt(target_timestamp_ms)
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();

        std::fs::write(&transcript, format!("{filler}\n{target}\n")).unwrap();
        let cold = parse_claude_fixture(source_home.path());
        assert_eq!(
            cold.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 50,
            "cold scan must see both turns"
        );

        // Compaction: the target turn drops out of the live file, so the next
        // scan retains it from the cache instead of a live parse.
        std::fs::write(&transcript, format!("{filler}\n")).unwrap();
        let rewritten = parse_claude_fixture(source_home.path());
        assert_eq!(
            rewritten.iter().map(|m| m.tokens.output).sum::<i64>(),
            10 + 50,
            "the rewrite scan must retain the dropped turn"
        );

        // Tamper the now-persisted retained entry's date directly, standing
        // in for "this was cached while a different local timezone was
        // active" without depending on the test process's real timezone.
        let mut cache = message_cache::SourceMessageCache::load();
        // Locate the entry by provenance rather than by path.
        // `CachedPath` compares raw OS strings, so a lookup keyed on the path
        // this test constructed is sensitive to separator and short-name
        // differences that do not affect what the scanner actually stored.
        // Retention only ever populates `retained_keys` for Claude, so the key
        // alone identifies the entry without naming a namespace.
        let mut entry = cache.entries_for_test().into_iter()
            .find(|entry| entry.retained_keys.contains(target_dedup_key))
            .unwrap_or_else(|| {
                panic!(
                    "the rewrite scan must have persisted an entry holding {target_dedup_key} as retained; cache holds {:?}",
                    cache.entries_for_test().into_iter()
                        .map(|e| (e.messages.len(), e.retained_keys.len()))
                        .collect::<Vec<_>>()
                )
            })
            .clone();
        let target_message = entry
            .messages
            .iter_mut()
            .find(|m| m.dedup_key.as_deref() == Some(target_dedup_key))
            .expect("the retained target turn must be in the persisted entry");
        assert_ne!(
            target_message.date, "1999-01-01",
            "the fixture's real date must differ from the tampered sentinel, or this test proves nothing"
        );
        target_message.date = "1999-01-01".to_string();
        cache.insert(entry);
        cache.save_if_dirty();

        // A since/until-style filter, exactly like `get_hourly_report`'s,
        // that only a correctly-refreshed `date` can pass.
        let date_filter = move |m: &UnifiedMessage| m.date == expected_date;

        let materialized_filtered: Vec<UnifiedMessage> = parse_claude_fixture(source_home.path())
            .into_iter()
            .filter(|m| date_filter(m))
            .collect();
        assert_eq!(
            materialized_filtered
                .iter()
                .map(|m| m.tokens.output)
                .sum::<i64>(),
            10 + 50,
            "the materialized lane must refresh the retained turn's date before filtering, \
             independent of what was persisted"
        );

        let mut streaming_filtered: Vec<UnifiedMessage> = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["claude".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &date_filter,
            &mut |message| streaming_filtered.push(message.clone()),
        );
        assert_eq!(
            streaming_filtered
                .iter()
                .map(|m| m.tokens.output)
                .sum::<i64>(),
            materialized_filtered
                .iter()
                .map(|m| m.tokens.output)
                .sum::<i64>(),
            "the streaming lane must agree with the materialized lane once both are filtered \
             on the retained turn's (refreshed) date"
        );
        assert!(
            streaming_filtered
                .iter()
                .any(|m| m.dedup_key.as_deref() == Some(target_dedup_key)),
            "the retained turn must survive the date filter in the streaming lane too, which \
             requires its date to have been refreshed from its timestamp before filtering, not \
             left at the stale persisted value"
        );
    }

    fn copilot_desktop_test_db(home: &Path) -> (PathBuf, rusqlite::Connection) {
        let root = home.join(".copilot");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("data.db");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE sessions (
                    id TEXT,
                    model TEXT,
                    total_input_tokens INTEGER,
                    total_output_tokens INTEGER,
                    total_cached_tokens INTEGER,
                    total_reasoning_tokens INTEGER,
                    total_nano_aiu INTEGER,
                    created_at TEXT,
                    agent TEXT
                );
                "#,
            )
            .unwrap();
        (path, connection)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_copilot_desktop_session(
        connection: &rusqlite::Connection,
        session_id: &str,
        model: &str,
        input: i64,
        output: i64,
        cache_read: i64,
        reasoning: i64,
        created_at: &str,
        agent: Option<&str>,
    ) {
        connection
            .execute(
                r#"
                INSERT INTO sessions (
                    id, model, total_input_tokens, total_output_tokens,
                    total_cached_tokens, total_reasoning_tokens, total_nano_aiu,
                    created_at, agent
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)
                "#,
                rusqlite::params![
                    session_id, model, input, output, cache_read, reasoning, created_at, agent
                ],
            )
            .unwrap();
    }

    fn write_copilot_otel_message(
        home: &Path,
        session_id: &str,
        unix_seconds: i64,
        input: i64,
        output: i64,
        agent: Option<&str>,
    ) -> PathBuf {
        let root = home.join(".copilot/otel");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("copilot.jsonl");
        let record = serde_json::json!({
            "type": "span",
            "traceId": format!("trace-{session_id}"),
            "spanId": format!("span-{session_id}"),
            "name": "chat gpt-5.4-mini",
            "startTime": [unix_seconds, 0],
            "attributes": {
                "gen_ai.operation.name": "chat",
                "gen_ai.response.model": "gpt-5.4-mini",
                "gen_ai.conversation.id": session_id,
                "gen_ai.usage.input_tokens": input,
                "gen_ai.usage.output_tokens": output,
                "gen_ai.agent.id": agent,
            }
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
        path
    }

    fn write_copilot_desktop_events(
        home: &Path,
        session_id: &str,
        model: &str,
        workspace: &str,
    ) -> PathBuf {
        let root = home.join(".copilot/session-state").join(session_id);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("events.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session.start\",\"data\":{{\"context\":{{\"cwd\":\"{workspace}\"}}}}}}\n{{\"type\":\"session.model_change\",\"data\":{{\"newModel\":\"{model}\"}}}}\n"
            ),
        )
        .unwrap();
        path
    }

    fn copilot_local_options(home: &Path) -> LocalParseOptions {
        LocalParseOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["copilot".to_string()]),
            ..Default::default()
        }
    }

    fn copilot_report_options(home: &Path) -> ReportOptions {
        ReportOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["copilot".to_string()]),
            ..Default::default()
        }
    }

    #[test]
    fn copilot_desktop_selector_prefers_otel_per_session() {
        let message = |session_id: &str, dedup_key: &str, input: i64| {
            UnifiedMessage::new_with_dedup(
                "copilot",
                "gpt-5.4-mini",
                "openai",
                session_id,
                1_782_909_296_000,
                TokenBreakdown {
                    input,
                    ..Default::default()
                },
                0.0,
                Some(dedup_key.to_string()),
            )
        };
        let selected = super::prefer_copilot_otel_messages(
            vec![message("shared", "otel:shared", 10)],
            vec![
                message("shared", "copilot-desktop:shared", 100),
                message("desktop-only", "copilot-desktop:desktop-only", 20),
            ],
        );

        assert_eq!(selected.len(), 2);
        assert!(selected
            .iter()
            .any(|message| message.dedup_key.as_deref() == Some("otel:shared")));
        assert!(selected.iter().any(|message| {
            message.dedup_key.as_deref() == Some("copilot-desktop:desktop-only")
        }));
        assert!(!selected
            .iter()
            .any(|message| { message.dedup_key.as_deref() == Some("copilot-desktop:shared") }));
    }

    #[test]
    #[serial_test::serial]
    fn copilot_desktop_authority_precedes_date_filters_in_all_consumers() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (_, connection) = copilot_desktop_test_db(source_home.path());
        insert_copilot_desktop_session(
            &connection,
            "shared",
            "gpt-5.4-mini",
            100,
            10,
            0,
            0,
            "2026-07-01T12:34:56Z",
            Some("github.copilot.default"),
        );
        drop(connection);
        write_copilot_otel_message(
            source_home.path(),
            "shared",
            1_735_689_600,
            10,
            1,
            Some("github.copilot.default"),
        );

        let mut options = copilot_local_options(source_home.path());
        options.since = Some("2026-07-01".to_string());
        options.until = Some("2026-07-01".to_string());
        let home = source_home.path().to_str().unwrap();
        let clients = ["copilot".to_string()];
        let materialized =
            super::parse_local_unified_messages_resolved(options.clone(), home, &clients, None)
                .unwrap();
        assert!(materialized.is_empty());

        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|message| message.date == "2026-07-01",
            &mut |message| streamed.push(message.clone()),
        );
        assert!(streamed.is_empty());

        let counted = parse_local_clients(options).unwrap();
        assert!(counted.messages.is_empty());
        assert_eq!(counted.counts.get(ClientId::Copilot), 1);
    }

    #[test]
    #[serial_test::serial]
    fn copilot_desktop_cache_tracks_events_and_wal_updates() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        insert_copilot_desktop_session(
            &connection,
            "session-1",
            "auto",
            100,
            10,
            0,
            0,
            "2026-07-01T12:34:56Z",
            Some("github.copilot.default"),
        );
        let home = source_home.path().to_str().unwrap();
        let clients = ["copilot".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_db_path = scanner_spelling(
            std::slice::from_ref(
                scan_result
                    .copilot_desktop_db
                    .as_ref()
                    .expect("Copilot Desktop database must be discovered"),
            ),
            &db_path,
        );

        let initial_fingerprint =
            message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        let initial_token =
            local_source_change_token(&copilot_local_options(source_home.path())).unwrap();
        let cold = parse_all_messages_with_pricing(home, &clients, None);
        let warm = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(cold, warm);
        assert_eq!(warm.len(), 1);
        assert_eq!(warm[0].model_id, "auto");

        let events = write_copilot_desktop_events(
            source_home.path(),
            "session-1",
            "claude-sonnet-4-5",
            "/tmp/copilot-workspace",
        );
        let event_fingerprint =
            message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        assert_ne!(event_fingerprint, initial_fingerprint);
        assert_ne!(
            local_source_change_token(&copilot_local_options(source_home.path())).unwrap(),
            initial_token
        );
        let event_refresh = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(event_refresh.len(), 1);
        assert_eq!(event_refresh[0].model_id, "claude-sonnet-4-5");
        assert_eq!(
            event_refresh[0].workspace_label.as_deref(),
            Some("copilot-workspace")
        );

        let event_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        std::fs::File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_modified(event_time)
            .unwrap();
        assert_eq!(
            latest_source_mtime_ms(&copilot_local_options(source_home.path())).unwrap(),
            1_800_000_000_000
        );

        let before_wal =
            message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        insert_copilot_desktop_session(
            &connection,
            "session-2",
            "gpt-5.4-mini",
            20,
            2,
            0,
            0,
            "2026-07-01T13:00:00Z",
            None,
        );
        let after_wal =
            message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        assert_ne!(after_wal, before_wal);
        let wal_refresh = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(wal_refresh.len(), 2);

        let cache = message_cache::SourceMessageCache::load();
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Copilot),
                &cache_db_path,
            )
            .is_some_and(|entry| entry.messages.iter().all(|message| message.cost == 0.0)));
    }

    #[test]
    fn copilot_desktop_modified_after_keeps_otel_suppressors_and_fails_open() {
        let source_home = tempfile::TempDir::new().unwrap();
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        drop(connection);
        let otel_path =
            write_copilot_otel_message(source_home.path(), "shared", 1_735_689_600, 10, 1, None);
        let events = write_copilot_desktop_events(
            source_home.path(),
            "shared",
            "gpt-5.4-mini",
            "/tmp/workspace",
        );
        let stale_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        std::fs::File::options()
            .write(true)
            .open(&db_path)
            .unwrap()
            .set_modified(stale_time)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&otel_path)
            .unwrap()
            .set_modified(stale_time)
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_modified(fresh_time)
            .unwrap();

        let mut event_fresh = scanner::ScanResult {
            copilot_desktop_db: Some(db_path.clone()),
            ..Default::default()
        };
        event_fresh
            .get_mut(ClientId::Copilot)
            .push(otel_path.clone());
        prune_scan_result_by_mtime(&mut event_fresh, 1_700_043_200_000);
        assert_eq!(event_fresh.copilot_desktop_db.as_ref(), Some(&db_path));
        assert_eq!(event_fresh.get(ClientId::Copilot), &vec![otel_path.clone()]);

        std::fs::File::options()
            .write(true)
            .open(&events)
            .unwrap()
            .set_modified(stale_time)
            .unwrap();
        let mut all_stale = scanner::ScanResult {
            copilot_desktop_db: Some(db_path),
            ..Default::default()
        };
        all_stale.get_mut(ClientId::Copilot).push(otel_path);
        prune_scan_result_by_mtime(&mut all_stale, 1_700_043_200_000);
        assert!(all_stale.copilot_desktop_db.is_none());
        assert!(all_stale.get(ClientId::Copilot).is_empty());

        let mut stat_failure = scanner::ScanResult {
            copilot_desktop_db: Some(source_home.path().join("missing.db")),
            ..Default::default()
        };
        stat_failure
            .get_mut(ClientId::Copilot)
            .push(source_home.path().join("missing-otel.jsonl"));
        prune_scan_result_by_mtime(&mut stat_failure, u64::MAX);
        assert!(stat_failure.copilot_desktop_db.is_some());
        assert_eq!(stat_failure.get(ClientId::Copilot).len(), 1);
    }

    #[test]
    fn copilot_desktop_dependency_probe_failure_fails_open() {
        let source_home = tempfile::TempDir::new().unwrap();
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        drop(connection);
        let session_state = source_home.path().join(".copilot/session-state");
        std::fs::create_dir_all(&session_state).unwrap();
        std::fs::File::create(session_state.join("not-a-directory")).unwrap();
        let otel_path =
            write_copilot_otel_message(source_home.path(), "shared", 1_735_689_600, 10, 1, None);

        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_none());
        assert!(local_source_change_token(&copilot_local_options(source_home.path())).is_err());

        let mut scan_result = scanner::ScanResult {
            copilot_desktop_db: Some(db_path.clone()),
            ..Default::default()
        };
        scan_result
            .get_mut(ClientId::Copilot)
            .push(otel_path.clone());
        prune_scan_result_by_mtime(&mut scan_result, u64::MAX);

        assert_eq!(scan_result.copilot_desktop_db.as_ref(), Some(&db_path));
        assert_eq!(scan_result.get(ClientId::Copilot), &vec![otel_path]);
    }

    #[cfg(unix)]
    #[test]
    fn copilot_desktop_unreadable_event_bypasses_caches_and_pruning() {
        use std::os::unix::fs::PermissionsExt;

        let source_home = tempfile::TempDir::new().unwrap();
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        insert_copilot_desktop_session(
            &connection,
            "shared",
            "auto",
            100,
            10,
            0,
            0,
            "2026-07-01T12:34:56Z",
            None,
        );
        drop(connection);
        let events = write_copilot_desktop_events(
            source_home.path(),
            "shared",
            "gpt-5.4-mini",
            "/tmp/workspace",
        );
        let otel_path =
            write_copilot_otel_message(source_home.path(), "shared", 1_735_689_600, 10, 1, None);
        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_some());
        assert!(local_source_change_token(&copilot_local_options(source_home.path())).is_ok());

        std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_none());
        assert!(local_source_change_token(&copilot_local_options(source_home.path())).is_err());

        let mut scan_result = scanner::ScanResult {
            copilot_desktop_db: Some(db_path.clone()),
            ..Default::default()
        };
        scan_result
            .get_mut(ClientId::Copilot)
            .push(otel_path.clone());
        prune_scan_result_by_mtime(&mut scan_result, u64::MAX);
        std::fs::set_permissions(&events, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(scan_result.copilot_desktop_db.as_ref(), Some(&db_path));
        assert_eq!(scan_result.get(ClientId::Copilot), &vec![otel_path]);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn copilot_desktop_unreadable_wal_never_reuses_db_only_cache() {
        use std::os::unix::fs::PermissionsExt;

        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        insert_copilot_desktop_session(
            &connection,
            "one",
            "gpt-5.4-mini",
            10,
            1,
            0,
            0,
            "2026-07-01T12:34:56Z",
            None,
        );
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
        let mut wal = db_path.clone().into_os_string();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        if wal.exists() {
            std::fs::remove_file(&wal).unwrap();
        }

        let home = source_home.path().to_str().unwrap();
        let clients = ["copilot".to_string()];
        assert_eq!(
            parse_all_messages_with_pricing(home, &clients, None).len(),
            1
        );
        let db_only_fingerprint =
            message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        let mut sentinel = parse_all_messages_with_pricing(home, &clients, None).remove(0);
        sentinel.model_id = "cached-sentinel".to_string();
        sentinel.tokens.input = 999;
        let mut cache = message_cache::SourceMessageCache::load();
        cache.insert(message_cache::CachedSourceEntry::new(
            message_cache::CacheIdentity::for_client(ClientId::Copilot),
            &db_path,
            db_only_fingerprint,
            vec![sentinel],
            Vec::new(),
            None,
        ));
        cache.save_if_dirty();

        let writer = rusqlite::Connection::open(&db_path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        insert_copilot_desktop_session(
            &writer,
            "two",
            "gpt-5.4-mini",
            20,
            2,
            0,
            0,
            "2026-07-01T13:00:00Z",
            None,
        );
        assert!(wal.is_file());
        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_some());
        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o000)).unwrap();

        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_none());
        assert!(local_source_change_token(&copilot_local_options(source_home.path())).is_err());
        assert!(copilot_desktop_source_mtime_ms(&db_path).is_none());
        assert!(
            super::collect_local_unified_messages(
                home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
            .is_err(),
            "an unreadable WAL must reject synchronization, never replay the sentinel"
        );
        let mut streamed = Vec::new();
        let result = super::scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert!(result.is_err());
        assert!(!streamed
            .iter()
            .any(|message| message.model_id == "cached-sentinel"));

        std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
        drop(writer);
        assert_eq!(
            parse_all_messages_with_pricing(home, &clients, None).len(),
            2
        );

        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(message_cache::SourceFingerprint::from_copilot_desktop_path(&db_path).is_none());
        assert!(local_source_change_token(&copilot_local_options(source_home.path())).is_err());
        assert!(copilot_desktop_source_mtime_ms(&db_path).is_none());
        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn copilot_desktop_materialized_streaming_count_and_report_parity() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (db_path, connection) = copilot_desktop_test_db(source_home.path());
        insert_copilot_desktop_session(
            &connection,
            "shared",
            "gpt-5.4-mini",
            100,
            10,
            0,
            0,
            "2026-07-01T12:34:56Z",
            Some("github.copilot.default"),
        );
        insert_copilot_desktop_session(
            &connection,
            "desktop-only",
            "gpt-5.4-mini",
            40,
            10,
            0,
            0,
            "2026-07-01T13:00:00Z",
            Some("github.copilot.default"),
        );
        drop(connection);
        let otel_path = write_copilot_otel_message(
            source_home.path(),
            "shared",
            1_782_909_296,
            20,
            5,
            Some("github.copilot.default"),
        );
        let home = source_home.path().to_str().unwrap();
        let clients = ["copilot".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_db_path = scanner_spelling(
            std::slice::from_ref(
                scan_result
                    .copilot_desktop_db
                    .as_ref()
                    .expect("Copilot Desktop database must be discovered"),
            ),
            &db_path,
        );
        let cache_otel_path = scanner_spelling(scan_result.get(ClientId::Copilot), &otel_path);

        let mut materialized = parse_all_messages_with_pricing(home, &clients, None);
        let cold_materialized = materialized.clone();
        let mut warm_materialized = parse_all_messages_with_pricing(home, &clients, None);
        materialized.sort_unstable_by(|left, right| left.session_id.cmp(&right.session_id));
        warm_materialized.sort_unstable_by(|left, right| left.session_id.cmp(&right.session_id));
        assert_eq!(materialized, warm_materialized);
        assert_eq!(materialized.len(), 2);
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            60
        );
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.output)
                .sum::<i64>(),
            15
        );
        assert!(!materialized
            .iter()
            .any(|message| { message.dedup_key.as_deref() == Some("copilot-desktop:shared") }));

        let cache = message_cache::SourceMessageCache::load();
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Copilot),
                &cache_db_path,
            )
            .is_some_and(|entry| entry.messages.len() == 2));
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Copilot),
                &cache_otel_path,
            )
            .is_some_and(|entry| entry.messages.len() == 1));

        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        streamed.sort_unstable_by(|left, right| left.session_id.cmp(&right.session_id));
        assert_eq!(streamed, materialized);

        let counted = parse_local_clients(copilot_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Copilot), 2);
        assert_eq!(counted.messages.len(), 2);
        for message in &counted.messages {
            let source = materialized
                .iter()
                .find(|source| source.session_id == message.session_id)
                .unwrap();
            assert_eq!(message.client, source.client);
            assert_eq!(message.model_id, source.model_id);
            assert_eq!(message.provider_id, source.provider_id);
            assert_eq!(message.workspace_key, source.workspace_key);
            assert_eq!(message.workspace_label, source.workspace_label);
            assert_eq!(message.timestamp, source.timestamp);
            assert_eq!(message.date, source.date);
            assert_eq!(message.input, source.tokens.input);
            assert_eq!(message.output, source.tokens.output);
            assert_eq!(message.cache_read, source.tokens.cache_read);
            assert_eq!(message.cache_write, source.tokens.cache_write);
            assert_eq!(message.reasoning, source.tokens.reasoning);
            assert_eq!(message.duration_ms, source.duration_ms);
            assert_eq!(message.message_count, source.message_count);
            assert_eq!(message.agent, source.agent);
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = copilot_report_options(source_home.path());
        let graph = runtime
            .block_on(super::generate_graph_with_loaded_pricing(
                options.clone(),
                None,
                super::GraphPricingMode::BestEffort,
            ))
            .unwrap()
            .into_graph();
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        let expected_tokens = cold_materialized
            .iter()
            .map(|message| message.tokens.total())
            .sum::<i64>();

        assert_eq!(graph.summary.total_tokens, expected_tokens);
        assert_eq!(
            graph
                .contributions
                .iter()
                .map(|entry| entry.totals.messages)
                .sum::<i32>(),
            2
        );
        assert_eq!(model.total_messages, 2);
        assert_eq!(model.total_input, 60);
        assert_eq!(model.total_output, 15);
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(agents.total_messages, 2);
        assert_eq!(
            agents.entries.iter().map(|entry| entry.input).sum::<i64>(),
            60
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_env_guard_restores_some_and_none_after_panic() {
        const KEYS: [&str; 3] = ["HOME", "TOKSCALE_PRICING_CACHE_ONLY", "TOKSCALE_CONFIG_DIR"];
        let _original = EnvGuard::capture(&KEYS);

        unsafe {
            std::env::set_var("HOME", "/tmp/tokscale-env-guard-home-before");
            std::env::remove_var("TOKSCALE_PRICING_CACHE_ONLY");
            std::env::set_var(
                "TOKSCALE_CONFIG_DIR",
                "/tmp/tokscale-env-guard-config-before",
            );
        }
        let first = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set(&[
                (
                    "HOME",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-home-during"),
                ),
                ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
                (
                    "TOKSCALE_CONFIG_DIR",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-config-during"),
                ),
            ]);
            panic!("exercise EnvGuard unwinding");
        });
        assert!(first.is_err());
        assert_eq!(
            std::env::var_os("HOME"),
            Some(std::ffi::OsString::from(
                "/tmp/tokscale-env-guard-home-before"
            ))
        );
        assert_eq!(std::env::var_os("TOKSCALE_PRICING_CACHE_ONLY"), None);
        assert_eq!(
            std::env::var_os("TOKSCALE_CONFIG_DIR"),
            Some(std::ffi::OsString::from(
                "/tmp/tokscale-env-guard-config-before"
            ))
        );

        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("TOKSCALE_PRICING_CACHE_ONLY", "before");
            std::env::remove_var("TOKSCALE_CONFIG_DIR");
        }
        let second = std::panic::catch_unwind(|| {
            let _guard = EnvGuard::set(&[
                (
                    "HOME",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-home-during"),
                ),
                ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
                (
                    "TOKSCALE_CONFIG_DIR",
                    std::ffi::OsStr::new("/tmp/tokscale-env-guard-config-during"),
                ),
            ]);
            panic!("exercise inverse EnvGuard unwinding");
        });
        assert!(second.is_err());
        assert_eq!(std::env::var_os("HOME"), None);
        assert_eq!(
            std::env::var_os("TOKSCALE_PRICING_CACHE_ONLY"),
            Some(std::ffi::OsString::from("before"))
        );
        assert_eq!(std::env::var_os("TOKSCALE_CONFIG_DIR"), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_empty_reports_normalize_total_cost_to_positive_zero() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);

        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            ..Default::default()
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let totals = [
            runtime
                .block_on(get_model_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_monthly_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_hourly_report(options.clone()))
                .unwrap()
                .total_cost,
            runtime
                .block_on(get_agents_report(options))
                .unwrap()
                .total_cost,
        ];

        for total in totals {
            assert_eq!(total.to_bits(), 0.0f64.to_bits());
        }
    }

    fn make_opencode_selection_message(key: &str, cost: f64, source: CostSource) -> UnifiedMessage {
        let mut message = UnifiedMessage::new_with_dedup(
            "opencode",
            "gpt-4o",
            "openai",
            "oc-session",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            cost,
            Some(key.to_string()),
        );
        match source {
            CostSource::ProviderReported => message.mark_provider_reported_cost(),
            CostSource::Estimated => message.mark_estimated_cost(),
            CostSource::PartiallyEstimated => message.mark_partially_estimated_cost(),
            CostSource::Unknown => {}
        }
        message
    }

    fn opencode_authority_set(key: &str) -> HashSet<OpenCodeSourceIdentity> {
        let message = make_opencode_selection_message(key, 0.0, CostSource::ProviderReported);
        OpenCodeSourceIdentity::all_from_message(&message)
            .into_iter()
            .collect()
    }

    #[test]
    fn test_opencode_streaming_selection_flushes_snapshot_fallback_on_json_drift() {
        // A missing file and an invalid file both produce no second-pass message;
        // a downgraded file produces an estimated message. All must flush SQLite.
        for second_pass in [None, None, Some(CostSource::Estimated)] {
            let key = "snapshot-authoritative";
            let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
            assert!(selection
                .select_sqlite(make_opencode_selection_message(
                    key,
                    0.25,
                    CostSource::Estimated,
                ))
                .is_none());
            if let Some(source) = second_pass {
                assert!(selection
                    .select_json(make_opencode_selection_message(key, 0.0, source,), true)
                    .is_none());
            }
            let selected: Vec<_> = selection.finish().collect();
            assert_eq!(selected.len(), 1);
            assert_eq!(selected[0].cost, 0.25);
            assert_eq!(selected[0].cost_source, CostSource::Estimated);
        }
    }

    #[test]
    fn test_opencode_streaming_selection_replaces_deferred_sqlite_estimate() {
        let key = "sqlite-authoritative-replacement";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.25,
                CostSource::Estimated,
            ))
            .is_none());

        let selected = selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.50,
                CostSource::ProviderReported,
            ))
            .expect("a later authoritative SQLite message must replace the fallback");
        assert_eq!(selected.cost, 0.50);
        assert_eq!(selected.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_first_sqlite_estimate() {
        let key = "sqlite-estimated-first-wins";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.25,
                CostSource::Estimated,
            ))
            .is_none());
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.50,
                CostSource::Estimated,
            ))
            .is_none());

        let selected: Vec<_> = selection.finish().collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].cost, 0.25);
        assert_eq!(selected[0].cost_source, CostSource::Estimated);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_first_sqlite_authority() {
        let key = "sqlite-authoritative-first-wins";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.50,
                CostSource::ProviderReported,
            ))
            .expect("the first authoritative SQLite message must be selected");
        assert_eq!(first.cost, 0.50);
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.75,
                CostSource::ProviderReported,
            ))
            .is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    /// The local fallback above must not follow a bundle off the machine.
    ///
    /// Two copies of one OpenCode message: the SQLite copy carries an
    /// estimated cost on a plain model id, the JSON copy is authoritative on a
    /// gateway model id. A `synthetic` query matches on model and provider, so
    /// it selects the JSON copy and not its twin — which means the twin is
    /// filtered while the authoritative copy is not, the one shape that lets
    /// selection see a difference. If the report filter can reach the
    /// selection, the deferred SQLite copy survives and is emitted in place of
    /// the copy the query actually matched, and the streaming bundle stops
    /// agreeing with the pure fold over the same messages.
    #[test]
    #[serial_test::serial]
    fn test_remote_source_selection_is_independent_of_the_query_filter() {
        let roots = tempfile::TempDir::new().unwrap();
        let home = roots.path().join("home");
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), &home);
        // Both sides must read the same files: the remote context resolves the
        // XDG data root from the injected platform root, while the local parse
        // with `use_env_roots: false` uses `home/.local/share`.
        let xdg_data = home.join(".local/share");

        let session = "session-twin";
        let message_id = "msg-twin";
        let db_dir = xdg_data.join("opencode");
        std::fs::create_dir_all(&db_dir).unwrap();
        let conn = rusqlite::Connection::open(db_dir.join("opencode.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);
             CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, directory) VALUES (?1, ?2)",
            rusqlite::params![session, "/workspace"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                message_id,
                session,
                r#"{"role":"assistant","time":{"created":2208988800000},"model":{"id":"hf:deepseek-ai/DeepSeek-V3-0324","providerID":"unknown"},"cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}"#
            ],
        )
        .unwrap();
        drop(conn);

        let message_dir = xdg_data.join("opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join(format!("{message_id}.json")),
            format!(
                r#"{{"id":"{message_id}","sessionID":"{session}","role":"assistant","modelID":"claude-sonnet-4","providerID":"anthropic","cost":0.5,"tokens":{{"input":10,"output":5,"reasoning":0,"cache":{{"read":0,"write":0}}}},"time":{{"created":2208988800000}}}}"#
            ),
        )
        .unwrap();

        let query = crate::RemoteUsageQueryV1 {
            clients: vec!["synthetic".to_owned()],
            start_date: "2040-01-01".to_owned(),
            end_date_exclusive: "2040-01-02".to_owned(),
            timezone: "UTC".to_owned(),
            tzdb_revision: "2026c".to_owned(),
        };
        let settings = scanner::ScannerSettings::default();
        let config = roots.path().join("config");
        let cache = roots.path().join("source-cache");
        let fingerprint = crate::RemoteSourceContextV1::preview_fingerprint(
            &home, &config, &xdg_data, &cache, &settings,
        )
        .unwrap();
        let context = crate::RemoteSourceContextV1::new(
            &home,
            &config,
            &xdg_data,
            &cache,
            settings,
            crate::RemoteSourceScopeTokenV1::new(fingerprint, 1),
        )
        .unwrap();
        let pricing = crate::RemotePricingSnapshot::from_cache_root(roots.path().join("pricing"));
        let streamed =
            crate::aggregate_remote_usage_from_source_v1(&context, &query, &pricing).unwrap();

        let rows = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::parse_local_unified_messages_with_pricing(
                crate::LocalParseOptions {
                    home_dir: Some(home.to_string_lossy().into_owned()),
                    use_env_roots: false,
                    clients: Some(vec!["synthetic".to_owned()]),
                    scanner_settings: scanner::ScannerSettings::default(),
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        let pure = crate::remote_report::aggregate_remote_usage_v1(&rows, &query).unwrap();

        // The authoritative copy is not synthetic, so the query matches
        // neither it nor — once selection has consumed it — its twin. Both
        // bundles are therefore empty, which is also why this compares bytes:
        // a non-empty synthetic bundle would differ between the streaming and
        // materialized lanes over gateway label normalization, unrelated to
        // what this test is about.
        assert!(streamed.graph.is_empty(), "the deferred twin escaped");
        assert_eq!(
            serde_json::to_vec(&streamed).unwrap(),
            serde_json::to_vec(&pure).unwrap()
        );
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_fallback_until_json_is_emitted() {
        let key = "filtered-authoritative";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.25,
                CostSource::Estimated,
            ))
            .is_none());
        assert!(selection
            .select_json(
                make_opencode_selection_message(key, 0.50, CostSource::ProviderReported,),
                false
            )
            .is_none());
        let selected: Vec<_> = selection.finish().collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].cost, 0.25);
    }

    #[test]
    fn test_opencode_streaming_selection_does_not_double_count_new_authority() {
        let key = "newly-authoritative";
        let mut selection = OpenCodeSelection::new(HashSet::new());
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.25,
                CostSource::Estimated,
            ))
            .is_some());
        assert!(selection
            .select_json(
                make_opencode_selection_message(key, 0.50, CostSource::ProviderReported,),
                true
            )
            .is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_incompatible_same_id_rows() {
        let key = "reused-message-id";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = make_opencode_selection_message(key, 0.25, CostSource::Estimated);
        let mut second = make_opencode_selection_message(key, 0.50, CostSource::Estimated);
        second.tokens.input = 20;
        let json = make_opencode_selection_message(key, 0.75, CostSource::ProviderReported);

        assert!(selection.select_sqlite(first).is_none());
        assert!(selection.select_sqlite(second).is_none());
        assert!(selection.select_json(json, true).is_some());
        let deferred: Vec<_> = selection.finish().collect();
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_replaces_deferred_identity_after_other_emission() {
        let key = "reused-authoritative-id";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        let first = make_opencode_selection_message(key, 0.25, CostSource::ProviderReported);
        let mut second = make_opencode_selection_message(key, 0.50, CostSource::Estimated);
        second.tokens.input = 20;
        let mut json = make_opencode_selection_message(key, 0.75, CostSource::ProviderReported);
        json.tokens.input = 20;

        assert!(selection.select_sqlite(first).is_some());
        assert!(selection.select_sqlite(second).is_none());
        let selected = selection
            .select_json(json, true)
            .expect("JSON must replace its exact deferred identity");
        assert_eq!(selected.tokens.input, 20);
        assert_eq!(selected.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_expands_authority_through_aliases() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let pure = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        let mut hybrid = make_opencode_selection_message(embedded, 0.50, CostSource::Estimated);
        hybrid.dedup_aliases.push(fallback.to_string());
        let json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        let duplicate =
            make_opencode_selection_message(embedded, 1.0, CostSource::ProviderReported);
        let authoritative = opencode_authoritative_sources(
            [&pure, &hybrid, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(pure).is_none());
        assert!(selection.select_sqlite(hybrid).is_none());
        let selected = selection
            .select_json(json, true)
            .expect("JSON authority must replace the aliased SQLite identity");
        assert_eq!(selected.cost, 0.75);
        assert!(selection.select_json(duplicate, true).is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_keeps_aliases_on_sqlite_replacement() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let mut hybrid = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        hybrid.dedup_aliases.push(fallback.to_string());
        let sqlite = make_opencode_selection_message(embedded, 0.50, CostSource::ProviderReported);
        let json = make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        let authoritative = opencode_authoritative_sources(
            [&hybrid, &sqlite, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(hybrid).is_none());
        assert!(selection.select_sqlite(sqlite).is_some());
        assert!(selection.select_json(json, true).is_none());
        assert_eq!(selection.finish().count(), 0);
    }

    #[test]
    fn test_opencode_streaming_selection_propagates_emitted_aliases_to_deferred_rows() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let first = make_opencode_selection_message(embedded, 0.25, CostSource::ProviderReported);
        let mut deferred = make_opencode_selection_message(embedded, 0.50, CostSource::Estimated);
        deferred.tokens.input = 20;
        deferred.dedup_aliases.push(fallback.to_string());
        let mut json =
            make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        json.tokens.input = 30;
        let authoritative = opencode_authoritative_sources(
            [&first, &deferred, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(first).is_some());
        assert!(selection.select_sqlite(deferred).is_none());
        assert!(selection.select_json(json, true).is_none());
        let remaining: Vec<_> = selection.finish().collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_propagates_emitted_aliases_from_deferred_graph() {
        let embedded = "shared-embedded";
        let fallback = "legacy-message";
        let mut deferred = make_opencode_selection_message(embedded, 0.25, CostSource::Estimated);
        deferred.tokens.input = 20;
        deferred.dedup_aliases.push(fallback.to_string());
        let sqlite = make_opencode_selection_message(embedded, 0.50, CostSource::ProviderReported);
        let mut json =
            make_opencode_selection_message(fallback, 0.75, CostSource::ProviderReported);
        json.tokens.input = 30;
        let authoritative = opencode_authoritative_sources(
            [&deferred, &sqlite, &json]
                .into_iter()
                .map(opencode_identity_group),
        );
        let mut selection = OpenCodeSelection::new(authoritative);

        assert!(selection.select_sqlite(deferred).is_none());
        assert!(selection.select_sqlite(sqlite).is_some());
        assert!(selection.select_json(json, true).is_none());
        let remaining: Vec<_> = selection.finish().collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].tokens.input, 20);
    }

    #[test]
    fn test_opencode_streaming_selection_prefers_snapshot_authority() {
        let key = "stable-authoritative";
        let mut selection = OpenCodeSelection::new(opencode_authority_set(key));
        assert!(selection
            .select_sqlite(make_opencode_selection_message(
                key,
                0.25,
                CostSource::Estimated,
            ))
            .is_none());
        let json = selection
            .select_json(
                make_opencode_selection_message(key, 0.50, CostSource::ProviderReported),
                true,
            )
            .unwrap();
        assert_eq!(json.cost, 0.50);
        assert_eq!(json.cost_source, CostSource::ProviderReported);
        assert_eq!(selection.finish().count(), 0);
    }

    fn make_workspace_message(
        client: &str,
        model_id: &str,
        provider_id: &str,
        session_id: &str,
        cost: f64,
        workspace_key: Option<&str>,
        workspace_label: Option<&str>,
    ) -> UnifiedMessage {
        let mut msg = UnifiedMessage::new(
            client,
            model_id,
            provider_id,
            session_id,
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            cost,
        );
        msg.set_workspace(
            workspace_key.map(str::to_string),
            workspace_label.map(str::to_string),
        );
        msg
    }

    fn make_trae_message(
        session_id: &str,
        timestamp: i64,
        dedup_key: Option<&str>,
        cost: f64,
    ) -> UnifiedMessage {
        UnifiedMessage::new_with_dedup(
            "trae",
            "gpt-5.2",
            "openai",
            session_id,
            timestamp,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            cost,
            dedup_key.map(str::to_string),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_opencode_sqlite_payload(
        created_ms: f64,
        completed_ms: f64,
        input: i64,
        output: i64,
        reasoning: i64,
        cache_read: i64,
        cache_write: i64,
        cost: f64,
    ) -> String {
        format!(
            r#"{{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "cost": {cost},
                "tokens": {{
                    "input": {input},
                    "output": {output},
                    "reasoning": {reasoning},
                    "cache": {{ "read": {cache_read}, "write": {cache_write} }}
                }},
                "time": {{ "created": {created_ms}, "completed": {completed_ms} }},
                "mode": "build"
            }}"#
        )
    }

    fn create_opencode_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn create_opencode_v2_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL
            );
            CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn create_hermes_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                model TEXT,
                started_at REAL NOT NULL,
                message_count INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                billing_provider TEXT,
                estimated_cost_usd REAL,
                actual_cost_usd REAL
            );",
        )
        .unwrap();
        conn
    }

    fn create_zed_sqlite_db(db_path: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                summary TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_zed_thread(conn: &rusqlite::Connection, id: &str, model: &str) {
        let payload = format!(
            r#"{{
                "version": "0.3.0",
                "title": "Test thread",
                "updated_at": "2026-05-01T12:30:00Z",
                "request_token_usage": {{
                    "turn-1": {{
                        "input_tokens": 42,
                        "output_tokens": 7,
                        "cache_creation_input_tokens": 3,
                        "cache_read_input_tokens": 5
                    }}
                }},
                "model": {{
                    "provider": "zed.dev",
                    "model": "{model}"
                }},
                "imported": false
            }}"#
        );
        conn.execute(
            "INSERT INTO threads (id, summary, updated_at, data_type, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, "Test thread", "2026-05-01T12:30:00Z", "json", payload.as_bytes()],
        )
        .unwrap();
    }

    fn insert_hermes_session(
        conn: &rusqlite::Connection,
        id: &str,
        model: &str,
        message_count: i64,
        input_tokens: i64,
        output_tokens: i64,
        actual_cost_usd: f64,
    ) {
        conn.execute(
            "INSERT INTO sessions (
                id, source, model, started_at, message_count,
                input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                billing_provider, estimated_cost_usd, actual_cost_usd
            ) VALUES (?1, 'cli', ?2, 1775001102.0, ?3, ?4, ?5, 0, 0, 0, 'anthropic', NULL, ?6)",
            rusqlite::params![
                id,
                model,
                message_count,
                input_tokens,
                output_tokens,
                actual_cost_usd
            ],
        )
        .unwrap();
    }

    #[test]
    fn test_normalize_model_for_grouping() {
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-20251101"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-5-20250929"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-20250514"),
            "claude-sonnet-4"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.5"),
            "claude-opus-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4.5"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.6"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-6-sonnet"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-5-haiku"),
            "claude-haiku-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("anthropic/claude-4-6-opus"),
            "claude-opus-4-6"
        );

        assert_eq!(normalize_model_for_grouping("gpt-5.2"), "gpt-5.2");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(xhigh)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(high)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(minimal)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(auto)"), "gpt-5.4");
        assert_eq!(normalize_model_for_grouping("gpt-5.4(none)"), "gpt-5.4");
        assert_eq!(
            normalize_model_for_grouping("gpt-5.4(weirdgarbage)"),
            "gpt-5.4(weirdgarbage)"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4.5(high)"),
            "claude-sonnet-4-5"
        );
        assert_eq!(
            normalize_model_for_grouping("gemini-3-pro(auto)"),
            "gemini-3-pro"
        );
        assert_eq!(
            normalize_model_for_grouping("gemini-2.5-pro"),
            "gemini-2.5-pro"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-high"),
            "claude-opus-4-5-high"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-5-thinking-high"),
            "claude-opus-4-5-thinking-high"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-sonnet-4-5-high"),
            "claude-sonnet-4-5-high"
        );

        assert_eq!(
            normalize_model_for_grouping("claude-4-sonnet"),
            "claude-4-sonnet"
        );
        assert_eq!(
            normalize_model_for_grouping("claude-4-opus-thinking"),
            "claude-4-opus-thinking"
        );

        assert_eq!(normalize_model_for_grouping("big-pickle"), "big-pickle");
        assert_eq!(normalize_model_for_grouping("grok-code"), "grok-code");

        assert_eq!(
            normalize_model_for_grouping("claude-opus-4.5-20251101"),
            "claude-opus-4-5"
        );
    }

    fn test_alias_map(pairs: &[(&str, &str)]) -> ModelAliasMap {
        ModelAliasMap {
            entries: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// Syrtis #118: with no aliases installed, Grok Build's `grok-4.6-build`
    /// usage key and the bare `grok-4.6` merge into one model-report row whose
    /// tokens, messages and cost are the exact sums; the raw identity path and
    /// an unrelated model are untouched.
    #[test]
    fn grok_build_rows_group_under_the_bare_version_without_aliases() {
        let _guard = crate::model_alias::lock_global_alias_tests();
        clear_model_aliases();

        let message = |model: &str, input: i64, output: i64, cost: f64| {
            let mut m = UnifiedMessage::new(
                "grok",
                model,
                "xai",
                "s1",
                1_733_011_200_000,
                TokenBreakdown {
                    input,
                    output,
                    cache_read: 7,
                    cache_write: 0,
                    reasoning: 3,
                    cache_write_1h: 0,
                },
                cost,
            );
            m.mark_estimated_cost();
            m
        };
        let entries = aggregate_model_usage_entries(
            vec![
                message("grok-4.6-build", 100, 20, 0.25),
                message("grok-4.6", 50, 10, 0.5),
                message("grok-code-fast-1", 9, 1, 0.125),
            ],
            &GroupBy::Model,
        );

        let mut rows: Vec<_> = entries
            .iter()
            .map(|e| {
                (
                    e.model.as_str(),
                    e.input,
                    e.output,
                    e.cache_read,
                    e.reasoning,
                    e.message_count,
                    e.cost,
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        assert_eq!(
            rows,
            vec![
                ("grok-4.6", 150, 30, 14, 6, 2, 0.75),
                ("grok-code-fast-1", 9, 1, 7, 3, 1, 0.125),
            ]
        );
        assert_eq!(canonical_model_id("grok-4.6-build"), "grok-4.6-build");
    }

    #[test]
    fn model_aliases_fold_grouping_only_not_canonical_or_pricing() {
        let _guard = crate::model_alias::lock_global_alias_tests();
        clear_model_aliases();

        // Alias a channel-specific spelling onto the display/group key.
        set_model_aliases(&test_alias_map(&[(
            "claude-opus-4-8-cc",
            "claude-opus-4-8",
        )]));

        // Grouping sees the folded label.
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8"
        );
        // Raw identity path stays alias-free (syntactic only).
        assert_eq!(
            canonical_model_id("claude-opus-4-8-cc"),
            "claude-opus-4-8-cc"
        );

        // Pricing resolves the *raw* message model id, never the grouping label.
        // Custom pricing is registered only under the raw channel spelling; the
        // display/group key has no rate. After the alias fold, cost still uses
        // the raw path and remains non-zero.
        let mut custom = HashMap::new();
        custom.insert(
            "claude-opus-4-8-cc".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let service = pricing::PricingService::new_with_custom(
            pricing::custom::CustomPricing::from_models(custom),
            HashMap::new(),
            HashMap::new(),
        );
        let tokens = TokenBreakdown {
            input: 1000,
            output: 500,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            cache_write_1h: 0,
        };
        let raw_cost = service.calculate_cost_with_provider("claude-opus-4-8-cc", None, &tokens);
        let group_label_cost =
            service.calculate_cost_with_provider("claude-opus-4-8", None, &tokens);
        assert!(raw_cost > 0.0, "raw id must remain the pricing key");
        assert_eq!(
            group_label_cost, 0.0,
            "grouping label must not be used as the pricing key"
        );

        let mut msg = UnifiedMessage::new(
            "claude",
            "claude-opus-4-8-cc",
            "anthropic",
            "s1",
            1_733_011_200_000,
            tokens.clone(),
            0.0,
        );
        apply_pricing_if_available(&mut msg, Some(&service));
        assert!(
            (msg.cost - raw_cost).abs() < 1e-12,
            "apply_pricing must keep using the raw message model_id (got {}, expected {})",
            msg.cost,
            raw_cost
        );
        // Message identity is never rewritten by aliases.
        assert_eq!(msg.model_id, "claude-opus-4-8-cc");

        // Model-report grouping merges channel variants under the alias label,
        // while pre-computed costs simply sum (aliases never reprice).
        let mut other = UnifiedMessage::new(
            "claude",
            "claude-opus-4-8",
            "anthropic",
            "s2",
            1_733_011_200_001,
            tokens,
            1.5,
        );
        other.mark_estimated_cost();
        msg.cost = raw_cost;
        msg.mark_estimated_cost();
        let entries = aggregate_model_usage_entries(vec![msg, other], &GroupBy::Model);
        assert_eq!(
            entries.len(),
            1,
            "alias must fold both variants into one bucket"
        );
        assert_eq!(entries[0].model, "claude-opus-4-8");
        assert!(
            (entries[0].cost - (raw_cost + 1.5)).abs() < 1e-9,
            "merged cost is the sum of already-costed buckets, not a reprice"
        );

        // Graph/export client contribution keeps the alias-free raw identity.
        let graph = fold_messages_streaming(&[UnifiedMessage::new(
            "claude",
            "claude-opus-4-8-cc",
            "anthropic",
            "s1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.25,
        )]);
        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].clients.len(), 1);
        assert_eq!(graph[0].clients[0].model_id, "claude-opus-4-8-cc");

        clear_model_aliases();
    }

    #[test]
    fn model_alias_reload_invalidates_usage_data_consumers() {
        let _guard = crate::model_alias::lock_global_alias_tests();
        clear_model_aliases();

        static FIRES: AtomicUsize = AtomicUsize::new(0);
        register_usage_data_invalidation_hook(|| {
            FIRES.fetch_add(1, Ordering::SeqCst);
        });
        let baseline_fires = FIRES.load(Ordering::SeqCst);
        let gen0 = model_alias_generation();

        set_model_aliases(&test_alias_map(&[(
            "claude-opus-4-8-cc",
            "claude-opus-4-8",
        )]));
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8"
        );
        let gen1 = model_alias_generation();
        assert!(gen1 > gen0);

        // Reload replaces the map; consumers see the new fold on the next report.
        set_model_aliases(&test_alias_map(&[("gpt-5.5-cc", "gpt-5.5")]));
        assert_eq!(
            normalize_model_for_grouping("claude-opus-4-8-cc"),
            "claude-opus-4-8-cc",
            "previous alias must not survive a reload"
        );
        assert_eq!(normalize_model_for_grouping("gpt-5.5-cc"), "gpt-5.5");
        let gen2 = model_alias_generation();
        assert!(gen2 > gen1);

        clear_model_aliases();
        assert_eq!(normalize_model_for_grouping("gpt-5.5-cc"), "gpt-5.5-cc");
        assert!(model_alias_generation() > gen2);

        let after_fires = FIRES.load(Ordering::SeqCst);
        assert!(
            after_fires >= baseline_fires + 3,
            "set/reload/clear must each fire the usage-data invalidation hook \
             (baseline={baseline_fires}, after={after_fires})"
        );
    }

    #[test]
    fn aggregate_model_usage_entries_uses_fold_start_alias_snapshot() {
        // Codex P2: multi-message report folds must keep one alias config for
        // the whole fold. `aggregate_model_usage_entries` snapshots at start;
        // prove the snapshot pattern it uses is stable under mid-fold reload,
        // then that the real aggregator still merges under the fold-start map.
        let _guard = crate::model_alias::lock_global_alias_tests();
        clear_model_aliases();
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-b")]));

        // Same capture point as aggregate_model_usage_entries.
        let aliases = snapshot_grouping_aliases();
        assert_eq!(
            aliases.fold(normalize_syntactic("alias-a")),
            "canonical-b",
            "fold start sees A→B"
        );

        // Mid-fold mutation of the process-wide map.
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-other")]));
        assert_eq!(
            normalize_model_for_grouping("alias-a"),
            "canonical-other",
            "live per-call path sees the reloaded map"
        );
        // Every message in the fold keeps the snapshotted label.
        for _ in 0..3 {
            assert_eq!(
                aliases.fold(normalize_syntactic("alias-a")),
                "canonical-b",
                "snapshot must ignore mid-fold reload for the rest of the fold"
            );
        }

        // Re-install fold-start aliases and run the real aggregator: both
        // channel variants merge under the snapshotted canonical label.
        set_model_aliases(&test_alias_map(&[("alias-a", "canonical-b")]));
        let tokens = TokenBreakdown {
            input: 10,
            output: 5,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            cache_write_1h: 0,
        };
        let msg_a = UnifiedMessage::new(
            "claude",
            "alias-a",
            "anthropic",
            "s1",
            1_733_011_200_000,
            tokens.clone(),
            1.0,
        );
        let msg_b = UnifiedMessage::new(
            "claude",
            "canonical-b",
            "anthropic",
            "s2",
            1_733_011_200_001,
            tokens,
            2.0,
        );
        let entries = aggregate_model_usage_entries(vec![msg_a, msg_b], &GroupBy::Model);
        assert_eq!(
            entries.len(),
            1,
            "fold-start alias must merge both variants"
        );
        assert_eq!(entries[0].model, "canonical-b");
        assert!((entries[0].cost - 3.0).abs() < 1e-12);

        clear_model_aliases();
    }

    #[test]
    fn test_group_by_from_str_valid_values() {
        assert_eq!(GroupBy::from_str("model").unwrap(), GroupBy::Model);
        assert_eq!(
            GroupBy::from_str("client,model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(
            GroupBy::from_str("client-model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(
            GroupBy::from_str("client,provider,model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("client-provider-model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("workspace,model").unwrap(),
            GroupBy::WorkspaceModel
        );
        assert_eq!(
            GroupBy::from_str("workspace-model").unwrap(),
            GroupBy::WorkspaceModel
        );
        assert_eq!(GroupBy::from_str("session").unwrap(), GroupBy::Session);
        assert_eq!(
            GroupBy::from_str("session,model").unwrap(),
            GroupBy::Session
        );
        assert_eq!(
            GroupBy::from_str("session-model").unwrap(),
            GroupBy::Session
        );
        assert_eq!(
            GroupBy::from_str("client,session").unwrap(),
            GroupBy::ClientSession
        );
        assert_eq!(
            GroupBy::from_str("client,session,model").unwrap(),
            GroupBy::ClientSession
        );
        assert_eq!(
            GroupBy::from_str("client-session-model").unwrap(),
            GroupBy::ClientSession
        );
        assert!(GroupBy::from_str("unknown").is_err());
    }

    #[test]
    fn test_group_by_default_is_client_model() {
        assert_eq!(GroupBy::default(), GroupBy::ClientModel);
    }

    #[test]
    fn test_group_by_display_round_trips_with_from_str() {
        let variants = [
            GroupBy::Model,
            GroupBy::ClientModel,
            GroupBy::ClientProviderModel,
            GroupBy::WorkspaceModel,
            GroupBy::Session,
            GroupBy::ClientSession,
        ];

        for variant in variants {
            let rendered = variant.to_string();
            let parsed = GroupBy::from_str(&rendered).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn test_group_by_from_str_whitespace_handling() {
        assert_eq!(
            GroupBy::from_str("client, model").unwrap(),
            GroupBy::ClientModel
        );
        assert_eq!(GroupBy::from_str(" model ").unwrap(), GroupBy::Model);
        assert_eq!(
            GroupBy::from_str("client , provider , model").unwrap(),
            GroupBy::ClientProviderModel
        );
        assert_eq!(
            GroupBy::from_str("workspace, model").unwrap(),
            GroupBy::WorkspaceModel
        );
    }

    #[test]
    fn test_model_usage_performance_uses_only_timed_positive_token_messages() {
        let mut timed = make_workspace_message(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-1",
            0.0,
            None,
            None,
        );
        timed.tokens = TokenBreakdown {
            input: 100,
            output: 50,
            cache_read: 25,
            cache_write: 0,
            reasoning: 25,
            cache_write_1h: 0,
        };
        timed.duration_ms = Some(400);

        let mut untimed = make_workspace_message(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-2",
            0.0,
            None,
            None,
        );
        untimed.tokens = TokenBreakdown {
            input: 300,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
            cache_write_1h: 0,
        };

        let entries = aggregate_model_usage_entries(vec![timed, untimed], &GroupBy::ClientModel);

        assert_eq!(entries.len(), 1);
        let performance = &entries[0].performance;
        assert_eq!(performance.total_duration_ms, 400);
        assert_eq!(performance.timed_tokens, 200);
        assert_eq!(performance.sample_count, 1);
        assert_eq!(performance.ms_per_1k_tokens, Some(2000.0));
        assert!((performance.token_coverage - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn test_model_usage_performance_is_null_without_duration_samples() {
        let entries = aggregate_model_usage_entries(
            vec![make_workspace_message(
                "claude",
                "claude-sonnet-4-5",
                "anthropic",
                "session-1",
                0.0,
                None,
                None,
            )],
            &GroupBy::ClientModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].performance.ms_per_1k_tokens, None);
        assert_eq!(entries[0].performance.total_duration_ms, 0);
        assert_eq!(entries[0].performance.timed_tokens, 0);
        assert_eq!(entries[0].performance.token_coverage, 0.0);
    }

    #[test]
    fn test_workspace_model_grouping_merges_same_workspace_and_model() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.25,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "qwen",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.75,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "claude-sonnet-4-5");
        assert_eq!(entries[0].workspace_key.as_deref(), Some("/repo-a"));
        assert_eq!(entries[0].workspace_label.as_deref(), Some("repo-a"));
        assert_eq!(entries[0].cost, 4.0);
        assert_eq!(entries[0].message_count, 2);
        assert_eq!(entries[0].merged_clients.as_deref(), Some("claude, qwen"));
    }

    #[test]
    fn test_model_grouping_merges_anthropic_prefixed_claude_variant_with_canonical_model() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "anthropic/claude-4-6-sonnet",
                    "anthropic",
                    "session-1",
                    1.25,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-6",
                    "anthropic",
                    "session-2",
                    2.75,
                    Some("/repo-b"),
                    Some("repo-b"),
                ),
            ],
            &GroupBy::ClientModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "claude-sonnet-4-6");
        assert_eq!(entries[0].input, 20);
        assert_eq!(entries[0].output, 10);
        assert_eq!(entries[0].cost, 4.0);
        assert_eq!(entries[0].message_count, 2);
    }

    #[test]
    fn test_workspace_model_grouping_separates_different_workspaces() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    Some("/repo-a"),
                    Some("repo-a"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.0,
                    Some("/repo-b"),
                    Some("repo-b"),
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 2);
        let labels: HashSet<_> = entries
            .iter()
            .map(|entry| entry.workspace_label.as_deref().unwrap())
            .collect();
        assert_eq!(labels, HashSet::from(["repo-a", "repo-b"]));
    }

    #[test]
    fn test_workspace_model_grouping_uses_unknown_bucket_without_workspace_metadata() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    None,
                    None,
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    "2.0".parse().unwrap(),
                    None,
                    None,
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].workspace_key, None);
        assert_eq!(
            entries[0].workspace_label.as_deref(),
            Some(UNKNOWN_WORKSPACE_LABEL)
        );
        assert_eq!(entries[0].message_count, 2);
        assert_eq!(entries[0].cost, 3.0);
    }

    #[test]
    fn test_parsed_round_trip_preserves_workspace_metadata() {
        let mut unified = UnifiedMessage::new(
            "qwen",
            "qwen3.5-plus",
            "qwen",
            "session-1",
            1_742_390_400_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 2,
                cache_write: 0,
                reasoning: 1,
                cache_write_1h: 0,
            },
            1.25,
        );
        unified.set_workspace(
            Some("//server/share/demo-workspace".to_string()),
            Some("demo-workspace".to_string()),
        );
        unified.duration_ms = Some(2500);

        let parsed = unified_to_parsed(&unified);
        let round_tripped = parsed_to_unified(&parsed, 2.5);

        assert_eq!(
            round_tripped.workspace_key.as_deref(),
            Some("//server/share/demo-workspace")
        );
        assert_eq!(
            round_tripped.workspace_label.as_deref(),
            Some("demo-workspace")
        );
        assert_eq!(round_tripped.cost, 2.5);
        assert_eq!(round_tripped.duration_ms, Some(2500));
    }

    #[test]
    fn test_workspace_model_grouping_keeps_real_unknown_workspace_separate() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-1",
                    1.0,
                    Some("unknown-workspace"),
                    Some("unknown-workspace"),
                ),
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-2",
                    2.0,
                    None,
                    None,
                ),
            ],
            &GroupBy::WorkspaceModel,
        );

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|entry| {
            entry.workspace_key.as_deref() == Some("unknown-workspace")
                && entry.workspace_label.as_deref() == Some("unknown-workspace")
                && (entry.cost - 1.0).abs() < f64::EPSILON
        }));
        assert!(entries.iter().any(|entry| {
            entry.workspace_key.is_none()
                && entry.workspace_label.as_deref() == Some(UNKNOWN_WORKSPACE_LABEL)
                && (entry.cost - 2.0).abs() < f64::EPSILON
        }));
    }

    #[test]
    fn test_session_grouping_merges_same_session_and_model() {
        // Two messages with the same session_id + same model — should collapse
        // into one row regardless of the client that produced them, because
        // GroupBy::Session keys on (session_id, model) only.
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    1.25,
                    None,
                    None,
                ),
                make_workspace_message(
                    "amp",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    2.75,
                    None,
                    None,
                ),
            ],
            &GroupBy::Session,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id.as_deref(), Some("session-shared"));
        assert_eq!(entries[0].model, "claude-sonnet-4-5");
        assert!((entries[0].cost - 4.0).abs() < f64::EPSILON);
        assert_eq!(entries[0].message_count, 2);
        assert!(entries[0].workspace_key.is_none());
        assert!(entries[0].workspace_label.is_none());
        // Session grouping does not merge_clients into a comma list.
        assert!(entries[0].merged_clients.is_none());
    }

    #[test]
    fn test_session_grouping_separates_different_sessions() {
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message("codex", "gpt-5", "openai", "session-a", 1.0, None, None),
                make_workspace_message("codex", "gpt-5", "openai", "session-b", 2.0, None, None),
            ],
            &GroupBy::Session,
        );

        assert_eq!(entries.len(), 2);
        let session_ids: HashSet<_> = entries
            .iter()
            .map(|e| e.session_id.as_deref().unwrap())
            .collect();
        assert_eq!(session_ids, HashSet::from(["session-a", "session-b"]));
    }

    #[test]
    fn test_client_session_grouping_keeps_clients_separate() {
        // Same session_id seen by two different clients (unusual in practice
        // but possible if parsers collide on an id space). ClientSession
        // must yield two rows; Session would yield one (covered above).
        let entries = aggregate_model_usage_entries(
            vec![
                make_workspace_message(
                    "claude",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    1.0,
                    None,
                    None,
                ),
                make_workspace_message(
                    "amp",
                    "claude-sonnet-4-5-20250929",
                    "anthropic",
                    "session-shared",
                    3.0,
                    None,
                    None,
                ),
            ],
            &GroupBy::ClientSession,
        );

        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_eq!(entry.session_id.as_deref(), Some("session-shared"));
            assert!(entry.merged_clients.is_none());
        }
        let by_client: HashSet<_> = entries.iter().map(|e| e.client.as_str()).collect();
        assert_eq!(by_client, HashSet::from(["claude", "amp"]));
    }

    #[test]
    fn test_non_session_grouping_does_not_populate_session_id() {
        // Defensive: only Session/ClientSession variants should set the
        // session_id field on ModelUsage — every other group_by must leave
        // it None so the camelCase JSON output omits it via
        // `skip_serializing_if = "Option::is_none"`.
        for group_by in &[
            GroupBy::Model,
            GroupBy::ClientModel,
            GroupBy::ClientProviderModel,
            GroupBy::WorkspaceModel,
        ] {
            let entries = aggregate_model_usage_entries(
                vec![make_workspace_message(
                    "codex",
                    "gpt-5",
                    "openai",
                    "session-x",
                    1.0,
                    None,
                    None,
                )],
                group_by,
            );
            assert_eq!(entries.len(), 1);
            assert!(
                entries[0].session_id.is_none(),
                "session_id leaked into {:?} grouping",
                group_by
            );
        }
    }

    #[test]
    fn test_retain_for_requested_clients_keeps_original_client_matches() {
        let requested: HashSet<&str> = HashSet::from(["opencode"]);
        assert!(retain_for_requested_clients(
            "opencode",
            "gpt-4o",
            "anthropic",
            &requested
        ));
        assert!(!retain_for_requested_clients(
            "claude",
            "gpt-4o",
            "anthropic",
            &requested
        ));
    }

    #[test]
    fn test_retain_for_requested_clients_accepts_synthetic_gateway_traffic() {
        let requested: HashSet<&str> = HashSet::from(["synthetic"]);
        assert!(retain_for_requested_clients(
            "opencode",
            "hf:deepseek-ai/DeepSeek-V3-0324",
            "unknown",
            &requested
        ));
        assert!(retain_for_requested_clients(
            "synthetic",
            "deepseek-v3-0324",
            "synthetic",
            &requested
        ));
        assert!(!retain_for_requested_clients(
            "opencode",
            "gpt-4o",
            "anthropic",
            &requested
        ));
    }

    #[test]
    fn test_retain_for_requested_clients_preserves_kilo_split() {
        let kilocode_only: HashSet<&str> = HashSet::from(["kilocode"]);
        assert!(retain_for_requested_clients(
            "kilocode",
            "gpt-5",
            "openai",
            &kilocode_only
        ));
        assert!(!retain_for_requested_clients(
            "kilo",
            "gpt-5",
            "openai",
            &kilocode_only
        ));

        let kilo_only: HashSet<&str> = HashSet::from(["kilo"]);
        assert!(retain_for_requested_clients(
            "kilo", "gpt-5", "openai", &kilo_only
        ));
        assert!(!retain_for_requested_clients(
            "kilocode", "gpt-5", "openai", &kilo_only
        ));
    }

    /// Usage-events JSON (what the tokscale CLI writes today) must flow through
    /// scan and parse; before the JSON lane only `usage*.csv` was discovered, so
    /// a JSON-only cache produced no Cursor rows. When the same account also has
    /// a CSV, only the JSON is counted.
    #[test]
    #[serial_test::serial]
    fn test_cursor_usage_json_only_and_json_wins_over_csv() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = temp_dir.path().join(".config/tokscale/cursor-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let json = r#"{"totalUsageEventsCount":1,"usageEventsDisplay":[{"timestamp":"1760000000000","model":"gpt-5","kind":"k","conversationId":"c1","tokenUsage":{"inputTokens":11,"outputTokens":2,"cacheReadTokens":3,"totalCents":50}}]}"#;
        std::fs::write(cache.join("usage.json"), json).unwrap();
        std::fs::write(cache.join("usage.last-sync-attempt"), "1760000000").unwrap();
        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let parse = || {
            parse_all_messages_with_pricing(
                temp_dir.path().to_str().unwrap(),
                &["cursor".to_string()],
                Some(&pricing),
            )
        };

        let messages = parse();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "c1");
        assert_eq!(messages[0].tokens.input, 11);
        assert!((messages[0].cost - 0.5).abs() < 1e-9);

        // A legacy CSV for the same account must not be counted beside it.
        let csv = "Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\"2025-10-09T08:00:00.000Z\",\"Included\",\"gpt-5\",\"No\",\"0\",\"9\",\"0\",\"1\",\"10\",\"0.10\"";
        std::fs::write(cache.join("usage.csv"), csv).unwrap();
        let messages = parse();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].session_id, "c1");
    }

    #[test]
    #[serial_test::serial]
    fn test_cursor_parse_path_reprices_missing_cost_composer_1_5_rows() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cursor_cache_dir = temp_dir.path().join(".config/tokscale/cursor-cache");
        std::fs::create_dir_all(&cursor_cache_dir).unwrap();

        let csv = r#"Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"2026-03-04T12:00:00.000Z","Included","Composer 1.5","No","1200","1000","5000","2000","8000","Included""#;
        std::fs::write(cursor_cache_dir.join("usage.csv"), csv).unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["cursor".to_string()],
            Some(&pricing),
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "cursor");
        assert_eq!(messages[0].model_id, "Composer 1.5");
        assert!(messages[0].cost > 0.0);
        assert!(!messages[0].has_authoritative_cost());
    }

    fn write_kimi_repeated_status_fixture_at(session_dir: &Path) {
        std::fs::create_dir_all(session_dir).unwrap();
        std::fs::write(
            session_dir.join("wire.jsonl"),
            r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 10, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}
{"timestamp": 1770983420.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 20, "output": 2, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-progressive"}}}
{"timestamp": 1770983430.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 5, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "msg-distinct"}}}
{"timestamp": 1770983440.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 7, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}}}}
{"timestamp": 1770983450.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 8, "output": 1, "input_cache_read": 0, "input_cache_creation": 0}}}}"#,
        )
        .unwrap();
    }

    fn write_kimi_repeated_status_fixture(source_home: &Path) {
        write_kimi_repeated_status_fixture_at(
            &source_home.join(".kimi/sessions/group-1/session-1"),
        );
    }

    #[cfg(not(windows))]
    #[test]
    #[serial_test::serial]
    fn resolved_default_home_reaches_materialized_and_streaming_report_lanes() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", source_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        write_kimi_repeated_status_fixture(source_home.path());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let materialized = runtime
            .block_on(parse_local_unified_messages(LocalParseOptions {
                clients: Some(vec!["kimi".to_string()]),
                ..Default::default()
            }))
            .unwrap();
        let report = runtime
            .block_on(get_model_report(ReportOptions {
                clients: Some(vec!["kimi".to_string()]),
                group_by: GroupBy::ClientModel,
                ..Default::default()
            }))
            .unwrap();

        assert_eq!(materialized.len(), 4);
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            40
        );
        assert_eq!(report.total_input, 40);
        assert_eq!(report.total_output, 5);
        assert_eq!(report.total_messages, 4);
    }

    #[cfg(windows)]
    #[test]
    #[serial_test::serial]
    fn windows_invalid_posix_home_falls_back_across_shipping_lanes() {
        if std::env::var("GITHUB_ACTIONS").ok().as_deref() != Some("true") {
            eprintln!(
                "TS175-S1 Windows disposable-profile matrix skipped: GITHUB_ACTIONS is not exactly \"true\""
            );
            return;
        }

        let profile_home = dirs::home_dir().expect("GitHub-hosted Windows runner has a profile");
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", std::ffi::OsStr::new("/home/user")),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let sessions_root = profile_home.join(".kimi/sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let profile_group = tempfile::Builder::new()
            .prefix("tokscale-ts175-")
            .tempdir_in(&sessions_root)
            .unwrap();
        write_kimi_repeated_status_fixture_at(&profile_group.path().join("session-profile"));

        let clients = vec!["kimi".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        // Frozen 731a2dcc2589cbeaf82150933331b5b2b6b590ff returned HOME verbatim,
        // handing exactly "/home/user" to this resolved materialized seam.
        let frozen_materialized = parse_all_messages_with_pricing_with_env_strategy(
            "/home/user",
            &clients,
            None,
            false,
            &scanner_settings,
            None,
        );
        assert!(
            frozen_materialized.is_empty(),
            "the frozen raw POSIX HOME must not reach the native Windows profile fixture"
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let materialized = runtime
            .block_on(parse_local_unified_messages(LocalParseOptions {
                clients: Some(clients.clone()),
                ..Default::default()
            }))
            .unwrap();
        let report = runtime
            .block_on(get_model_report(ReportOptions {
                clients: Some(clients.clone()),
                group_by: GroupBy::ClientModel,
                ..Default::default()
            }))
            .unwrap();
        let resolved_home = super::get_home_dir_string(&None).unwrap();
        assert_eq!(PathBuf::from(&resolved_home), profile_home);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            &resolved_home,
            &clients,
            None,
            false,
            &scanner_settings,
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );

        let message_totals = |messages: &[UnifiedMessage]| {
            (
                messages.len(),
                messages
                    .iter()
                    .map(|message| message.tokens.input)
                    .sum::<i64>(),
                messages
                    .iter()
                    .map(|message| message.tokens.output)
                    .sum::<i64>(),
            )
        };
        // Legacy Kimi rows without a message_id have no dedup key, so the
        // preserved source timestamp distinguishes those otherwise-unkeyed rows.
        let source_identities = |messages: &[UnifiedMessage]| {
            messages
                .iter()
                .map(|message| {
                    (
                        message.client.clone(),
                        message.session_id.clone(),
                        message.timestamp,
                        message.dedup_key.clone(),
                    )
                })
                .collect::<HashSet<_>>()
        };

        assert_eq!(message_totals(&materialized), (4, 40, 5));
        assert_eq!(
            (
                report.total_input,
                report.total_output,
                report.total_messages
            ),
            (40, 5, 4)
        );
        assert_eq!(message_totals(&streamed), (4, 40, 5));
        assert!(materialized
            .iter()
            .all(|message| message.session_id == "session-profile"));
        let materialized_identities = source_identities(&materialized);
        assert_eq!(materialized_identities.len(), 4);
        assert_eq!(source_identities(&streamed), materialized_identities);

        let explicit_home = tempfile::TempDir::new().unwrap();
        assert!(explicit_home.path().is_absolute());
        write_kimi_repeated_status_fixture_at(
            &explicit_home
                .path()
                .join(".kimi/sessions/group-explicit/session-explicit"),
        );
        let explicit_home_string = explicit_home.path().to_string_lossy().into_owned();
        let explicit_materialized = runtime
            .block_on(parse_local_unified_messages(LocalParseOptions {
                home_dir: Some(explicit_home_string.clone()),
                clients: Some(clients.clone()),
                ..Default::default()
            }))
            .unwrap();
        let explicit_report = runtime
            .block_on(get_model_report(ReportOptions {
                home_dir: Some(explicit_home_string),
                clients: Some(clients),
                group_by: GroupBy::ClientModel,
                ..Default::default()
            }))
            .unwrap();

        assert_eq!(message_totals(&explicit_materialized), (4, 40, 5));
        assert!(
            explicit_materialized
                .iter()
                .all(|message| message.session_id == "session-explicit"),
            "the native explicit home must remain authoritative over the profile fallback"
        );
        assert_eq!(
            (
                explicit_report.total_input,
                explicit_report.total_output,
                explicit_report.total_messages,
            ),
            (40, 5, 4)
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_kimi_deduplicates_repeated_status_updates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_kimi_repeated_status_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["kimi".to_string()],
                None,
            );

            assert_eq!(messages.len(), 4);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 40);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 5);
        }
    }

    // Ported from upstream #1323: two Pi session files carrying the same
    // `responseId` with conflicting usage keep the first copy in scan-path
    // order, on the count lane; the materialized and streaming lanes must
    // agree with it.
    #[test]
    #[serial_test::serial]
    fn test_pi_fork_copies_dedup_first_wins_on_every_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let sessions_dir = source_home.path().join(".pi/agent/sessions/--fixture--");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let record = |session: &str, input: i64, output: i64| {
            let total = input + output;
            format!(
                r#"{{"type":"session","id":"{session}","timestamp":"2026-09-06T12:00:00.000Z","cwd":"/tmp/demo"}}"#,
            ) + "\n"
                + &format!(
                    r#"{{"type":"message","id":"entry-fork-copy","parentId":"{session}","timestamp":"2026-09-06T12:00:00.000Z","message":{{"role":"assistant","provider":"openai-codex","model":"gpt-6-astra","responseId":"resp-demo-fork","usage":{{"input":{input},"output":{output},"cacheRead":0,"cacheWrite":0,"totalTokens":{total}}}}}}}"#
                )
                + "\n"
        };
        std::fs::write(
            sessions_dir.join("session-a.jsonl"),
            record("session-a", 100, 20),
        )
        .unwrap();
        std::fs::write(
            sessions_dir.join("session-b.jsonl"),
            record("session-b", 999, 999),
        )
        .unwrap();

        for _ in 0..25 {
            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["pi".to_string()]),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(parsed.counts.get(ClientId::Pi), 1);
            assert_eq!(parsed.messages.len(), 1);
            assert_eq!(parsed.messages[0].input, 100);
            assert_eq!(parsed.messages[0].output, 20);
        }

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["pi".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].tokens.input, 100);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["pi".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_m: &UnifiedMessage| true,
            &mut |m: &UnifiedMessage| streamed.push(m.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].tokens.input, 100);
    }

    // Kimi Work (upstream #1170) is a second Kimi Code root, not a new client.
    // The same session reachable from both the Kimi Code and the Work root
    // must count once on every lane, through both the legacy scan and the
    // source-context scan; a Work-only session still counts.
    #[test]
    #[serial_test::serial]
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn test_kimi_work_and_kimi_code_copies_count_once_on_every_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let app_data = if cfg!(target_os = "macos") {
            source_home
                .path()
                .join("Library")
                .join("Application Support")
        } else {
            source_home.path().join("AppData").join("Roaming")
        };
        let work_sessions = app_data
            .join("kimi-desktop")
            .join("daimon-share")
            .join("daimon")
            .join("runtime")
            .join("kimi-code")
            .join("home")
            .join("sessions");
        let code_sessions = source_home.path().join(".kimi-code").join("sessions");
        let wire = |root: &Path, session: &str, input: i64| {
            let path = root
                .join("wd_workspace_c107cac82a87")
                .join(session)
                .join("agents")
                .join("main")
                .join("wire.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                format!(
                    r#"{{"type":"usage.record","model":"k2d6-agent","usage":{{"inputOther":{input},"output":10,"inputCacheRead":20,"inputCacheCreation":0}},"usageScope":"turn","time":1780319377010}}"#
                ),
            )
            .unwrap();
        };
        wire(&code_sessions, "conv-shared", 100);
        wire(&work_sessions, "conv-shared", 100);
        wire(&work_sessions, "conv-work-only", 7);

        let home = source_home.path().to_str().unwrap();
        let clients = ["kimi".to_string()];
        let inputs = |mut values: Vec<i64>| {
            values.sort_unstable();
            values
        };
        let expected = vec![7, 100];

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(
            inputs(materialized.iter().map(|m| m.tokens.input).collect()),
            expected
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_m: &UnifiedMessage| true,
            &mut |m: &UnifiedMessage| streamed.push(m.clone()),
        );
        assert_eq!(
            inputs(streamed.iter().map(|m| m.tokens.input).collect()),
            expected
        );

        let local_options = || LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        };
        let counted = parse_local_clients(local_options()).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kimi), 2);
        assert_eq!(
            inputs(counted.messages.iter().map(|m| m.input).collect()),
            expected
        );

        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let in_context = parse_local_clients_with_source_context(
            &context,
            LocalParseOptions {
                home_dir: None,
                ..local_options()
            },
        )
        .unwrap();
        assert_eq!(in_context.counts.get(ClientId::Kimi), 2);
        assert_eq!(
            inputs(in_context.messages.iter().map(|m| m.input).collect()),
            expected
        );
    }

    // An old zero-message entry must not hide a live BOM transcript with the
    // same fingerprint. Recover it once, then keep it visible in every lane.
    #[test]
    #[serial_test::serial]
    fn test_pi_bom_transcript_recovers_past_a_same_fingerprint_empty_entry() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            (
                "SYRTIS_USAGE_DIR",
                cache_home.path().join("usage").as_os_str(),
            ),
            (
                "SYRTIS_CONFIG_DIR",
                cache_home.path().join("syrtis-config").as_os_str(),
            ),
        ]);

        let sessions_dir = source_home.path().join(".pi/agent/sessions/--fixture--");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::write(
            sessions_dir.join("session-bom.jsonl"),
            concat!(
                "\u{feff}",
                r#"{"type":"session","id":"session-bom","timestamp":"2026-08-08T00:00:00.000Z","cwd":"/tmp/demo"}"#,
                "\n",
                r#"{"type":"message","id":"assistant-1","timestamp":"2026-08-08T00:00:01.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-5","usage":{"input":20,"output":8}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let home = source_home.path().to_str().unwrap();
        let clients = ["pi".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let path = scan_result.get(ClientId::Pi)[0].clone();
        let identity = message_cache::CacheIdentity::for_client(ClientId::Pi);
        let seed_empty_entry = || {
            let cache = message_cache::SourceMessageCache::load();
            cache
                .write_source(
                    identity,
                    &path,
                    message_cache::SourceFingerprint::from_path_samples_only(&path).unwrap(),
                    None,
                    None,
                    None,
                    &mut |_| {},
                    &mut |_, _| Ok(()),
                    |_| Ok(()),
                )
                .unwrap();
            let count = cache
                .metadata(identity, &path)
                .unwrap()
                .unwrap()
                .message_count;
            assert_eq!(count, 0, "the recovery fixture must begin without usage");
        };

        seed_empty_entry();
        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_m: &UnifiedMessage| true,
            &mut |m: &UnifiedMessage| streamed.push(m.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].session_id, "session-bom");
        assert_eq!(streamed[0].tokens.total(), 28);

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].session_id, "session-bom");
        assert_eq!(materialized[0].tokens.total(), 28);

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Pi), 1);
        assert_eq!(counted.messages[0].session_id, "session-bom");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_kimi_deduplicates_repeated_status_updates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_kimi_repeated_status_fixture(source_home.path());

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["kimi".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::Kimi), 4);
            assert_eq!(parsed.messages.len(), 4);
            assert_eq!(parsed.messages.iter().map(|m| m.input).sum::<i64>(), 40);
            assert_eq!(parsed.messages.iter().map(|m| m.output).sum::<i64>(), 5);
        }
    }

    // Regression: the streaming driver must NOT share one dedup set across
    // different clients. kimi and codebuff both emit raw upstream message ids
    // as dedup_key with no client namespace, so a shared set would let one
    // client's key suppress an identical key from the other. Here both a kimi
    // message and a codebuff message carry dedup_key "COLLIDE"; both must
    // survive. With a single shared `seen_keys` (the pre-fix behaviour) the
    // second lane's message is silently dropped and this fails.
    #[test]
    #[serial_test::serial]
    fn test_streaming_driver_does_not_dedup_across_clients() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            // kimi: one StatusUpdate carrying message_id "COLLIDE".
            let kimi_dir = source_home.path().join(".kimi/sessions/g/s");
            std::fs::create_dir_all(&kimi_dir).unwrap();
            std::fs::write(
                kimi_dir.join("wire.jsonl"),
                r#"{"type": "metadata", "protocol_version": "1.3"}
{"timestamp": 1770983410.0, "message": {"type": "StatusUpdate", "payload": {"token_usage": {"input_other": 100, "output": 50, "input_cache_read": 0, "input_cache_creation": 0}, "message_id": "COLLIDE"}}}"#,
            )
            .unwrap();

            // codebuff: one assistant message whose upstream id is "COLLIDE".
            let cb_dir = source_home.path().join(".config/manicode/projects/proj");
            std::fs::create_dir_all(&cb_dir).unwrap();
            std::fs::write(
                cb_dir.join("chat-messages.json"),
                r#"[{"role":"assistant","id":"COLLIDE","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
            )
            .unwrap();

            let mut seen: Vec<String> = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["kimi".to_string(), "codebuff".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| seen.push(m.client.clone()),
            );

            assert!(
                seen.iter().any(|c| c == "kimi"),
                "kimi message with shared dedup_key must survive: {seen:?}"
            );
            assert!(
                seen.iter().any(|c| c == "codebuff"),
                "codebuff message with shared dedup_key must survive: {seen:?}"
            );
        }
    }

    // M2 (codex fork-replay): the parser-level fork dedup (#649/#681) must also
    // collapse replayed parent token_count rows through OUR streaming report
    // path (scan_messages_streaming), not just the materialized
    // parse_all_messages_with_pricing path the upstream tests exercise. Without
    // the fork-parent-scoped dedup key, each fork's replayed parent rows survive
    // per child and inflate codex totals.
    #[test]
    #[serial_test::serial]
    fn test_streaming_codex_collapses_parent_replay_across_forks() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_parent_replay_fixture(source_home.path());

            let mut input_sum = 0i64;
            let mut output_sum = 0i64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    input_sum += m.tokens.input;
                    output_sum += m.tokens.output;
                    count += 1;
                },
            );

            // Same collapse as the materialized
            // test_parse_all_messages_with_pricing_codex_deduplicates_parent_replay_across_forks:
            // the parent's two turns plus the single own-turn shared (by identical
            // cumulative total) across the two forks. Without #649/#681 the
            // replayed parent rows would survive per fork and inflate this.
            assert_eq!(count, 3, "replayed parent rows must collapse to 3 messages");
            assert_eq!(input_sum, 140);
            assert_eq!(output_sum, 14);
        }
    }

    // Issue #6: the agents report must dedup the simple_lane! clients
    // (copilot/codebuff/kimi/…) like the model/graph/hourly reports. Here
    // codebuff emits the SAME upstream message id "DUP" in two different
    // project files. The OLD materialized path (parse_local_unified_messages)
    // never gated codebuff, so it counts both; the streaming-backed
    // get_agents_report keeps one — matching get_model_report. Repointing
    // get_agents_report at the old path makes the parity assertion FAIL (RED).
    #[test]
    #[serial_test::serial]
    fn test_agents_report_dedups_like_model_report_issue6() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        // Hermetic: cache-only pricing + temp HOME → no network, pricing None.
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        {
            let write_codebuff = |proj: &str| {
                let dir = source_home
                    .path()
                    .join(format!(".config/manicode/projects/{proj}"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("chat-messages.json"),
                    r#"[{"role":"assistant","id":"DUP","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
                )
                .unwrap();
            };
            write_codebuff("projA");
            write_codebuff("projB");

            let home = source_home.path().to_str().unwrap().to_string();
            let clients = Some(vec!["codebuff".to_string()]);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let agents = rt
                .block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();
            let model = rt
                .block_on(get_model_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();
            let old = rt
                .block_on(parse_local_unified_messages(LocalParseOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    since: None,
                    until: None,
                    year: None,
                    scanner_settings: scanner::ScannerSettings::default(),
                    modified_after: None,
                }))
                .unwrap();
            let old_total: i32 = old.iter().map(|m| m.message_count.max(0)).sum();

            assert_eq!(
                old_total, 2,
                "old materialized path must NOT dedup codebuff"
            );
            assert_eq!(model.total_messages, 1, "model report dedups codebuff");
            assert_eq!(
                agents.total_messages, 1,
                "agents report must dedup codebuff"
            );
            assert_eq!(
                agents.total_messages, model.total_messages,
                "issue #6: agents must agree with the model report"
            );
            assert_ne!(
                old_total, model.total_messages,
                "the old path diverged from the model report (the #6 bug)"
            );
            assert!(
                (agents.total_cost - model.total_cost).abs() < 1e-9,
                "agents/model cost parity (agents={}, model={})",
                agents.total_cost,
                model.total_cost
            );
        }
    }

    // Preservation: with no duplicate dedup_keys (and only parse_local==true
    // clients), the streaming-backed agents report produces the SAME numbers the
    // old materialized path did. codebuff + kimi, distinct ids, no agent
    // attribution → a single "Main" bucket.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_preserves_numbers_without_duplicates() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        {
            let cb_dir = source_home.path().join(".config/manicode/projects/proj");
            std::fs::create_dir_all(&cb_dir).unwrap();
            std::fs::write(
                cb_dir.join("chat-messages.json"),
                r#"[{"role":"assistant","id":"A","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
            )
            .unwrap();
            let kimi_dir = source_home.path().join(".kimi/sessions/g/s");
            std::fs::create_dir_all(&kimi_dir).unwrap();
            std::fs::write(
                kimi_dir.join("wire.jsonl"),
                "{\"type\": \"metadata\", \"protocol_version\": \"1.3\"}\n{\"timestamp\": 1770983410.0, \"message\": {\"type\": \"StatusUpdate\", \"payload\": {\"token_usage\": {\"input_other\": 100, \"output\": 50, \"input_cache_read\": 0, \"input_cache_creation\": 0}, \"message_id\": \"K\"}}}",
            )
            .unwrap();

            let home = source_home.path().to_str().unwrap().to_string();
            let clients = Some(vec!["codebuff".to_string(), "kimi".to_string()]);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let agents = rt
                .block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: clients.clone(),
                    ..Default::default()
                }))
                .unwrap();

            assert_eq!(
                agents.entries.len(),
                1,
                "no agent attribution → a single Main bucket"
            );
            let main = &agents.entries[0];
            assert_eq!(main.agent, "Main");
            assert_eq!(main.messages, 2);
            // BTreeSet → sorted, both clients fold into Main.
            assert_eq!(
                main.clients,
                vec!["codebuff".to_string(), "kimi".to_string()]
            );

            // Byte-for-byte equivalence with the old materialized path for the
            // non-duplicate case (both parse identically; only dedup differs).
            let old = rt
                .block_on(parse_local_unified_messages(LocalParseOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients,
                    since: None,
                    until: None,
                    year: None,
                    scanner_settings: scanner::ScannerSettings::default(),
                    modified_after: None,
                }))
                .unwrap();
            let old_input: i64 = old.iter().map(|m| m.tokens.input).sum();
            let old_output: i64 = old.iter().map(|m| m.tokens.output).sum();
            let old_cache_read: i64 = old.iter().map(|m| m.tokens.cache_read).sum();
            let old_cache_write: i64 = old.iter().map(|m| m.tokens.cache_write).sum();
            let old_reasoning: i64 = old.iter().map(|m| m.tokens.reasoning).sum();
            let old_messages: i32 = old.iter().map(|m| m.message_count.max(0)).sum();
            let old_cost: f64 = old.iter().map(|m| m.cost).sum();

            assert_eq!(main.input, old_input);
            assert_eq!(main.output, old_output);
            assert_eq!(main.cache_read, old_cache_read);
            assert_eq!(main.cache_write, old_cache_write);
            assert_eq!(main.reasoning, old_reasoning);
            assert_eq!(main.messages, old_messages);
            assert!((agents.total_cost - old_cost).abs() < 1e-9);
            // Sanity: codebuff contributes its known tokens.
            assert!(main.input >= 200 && main.output >= 80);
        }
    }

    // Issue #36: the client selection must be applied at the STREAMING SCAN,
    // not by a downstream membership filter over the pre-aggregated buckets.
    // codebuff (200/80) and kimi (100/50) fold into ONE shared "Main" agent
    // bucket; the FFI/DashboardModel now thread the selection into
    // ReportOptions.clients, so filtering to codebuff yields ONLY its 200/80 —
    // NOT the mixed 300/130. The old approach (unfiltered report + a Swift
    // membership filter over whole buckets) kept the entire shared bucket and
    // would read 300/130 here, so this test is RED against it and GREEN now.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_client_filter_scopes_shared_bucket_issue36() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        {
            let cb_dir = source_home.path().join(".config/manicode/projects/proj");
            std::fs::create_dir_all(&cb_dir).unwrap();
            std::fs::write(
                cb_dir.join("chat-messages.json"),
                r#"[{"role":"assistant","id":"A","metadata":{"model":"claude-sonnet-4","usage":{"inputTokens":200,"outputTokens":80}},"credits":0.02}]"#,
            )
            .unwrap();
            let kimi_dir = source_home.path().join(".kimi/sessions/g/s");
            std::fs::create_dir_all(&kimi_dir).unwrap();
            std::fs::write(
                kimi_dir.join("wire.jsonl"),
                "{\"type\": \"metadata\", \"protocol_version\": \"1.3\"}\n{\"timestamp\": 1770983410.0, \"message\": {\"type\": \"StatusUpdate\", \"payload\": {\"token_usage\": {\"input_other\": 100, \"output\": 50, \"input_cache_read\": 0, \"input_cache_creation\": 0}, \"message_id\": \"K\"}}}",
            )
            .unwrap();

            let home = source_home.path().to_str().unwrap().to_string();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let run = |clients: Option<Vec<String>>| {
                rt.block_on(get_agents_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients,
                    ..Default::default()
                }))
                .unwrap()
            };

            // All clients: one shared "Main" bucket carrying the mixed total.
            let all = run(None);
            assert_eq!(
                all.entries.len(),
                1,
                "codebuff + kimi share one Main bucket"
            );
            assert_eq!(all.entries[0].agent, "Main");
            assert_eq!(all.entries[0].input, 300, "mixed bucket = 200 + 100");
            assert_eq!(all.entries[0].output, 130, "mixed bucket = 80 + 50");
            assert_eq!(all.total_messages, 2);

            // Filtered to codebuff: the SAME shared bucket, scoped at the scan
            // to codebuff's contribution alone — proves the FFI-level filter,
            // not a whole-bucket membership keep (which would still read 300).
            let filtered = run(Some(vec!["codebuff".to_string()]));
            assert_eq!(filtered.entries.len(), 1);
            assert_eq!(filtered.entries[0].agent, "Main");
            assert_eq!(
                filtered.entries[0].input, 200,
                "filtered = codebuff only, not the mixed 300"
            );
            assert_eq!(filtered.entries[0].output, 80);
            assert_eq!(filtered.entries[0].clients, vec!["codebuff".to_string()]);
            assert_eq!(filtered.total_messages, 1, "kimi's message is gone");
        }
    }

    // Issue #36 (round 3): a cc-mirror variant id (`cc-mirror/kimi-code`) is
    // produced during CLAUDE-lane parsing, not by a scanner lane of its own —
    // `ClientId::from_str("cc-mirror/kimi-code")` is None. The two-level split
    // must (a) map the variant to its producing `claude` lane so the scan finds
    // it, and (b) keep ONLY the exact requested ids at fold time so requesting
    // the variant returns just the variant, and requesting `claude` returns
    // plain claude WITHOUT the variant (which the graph/daily/models surface as
    // its own client id). RED against the old lane-only filter: requesting the
    // variant returned empty, and requesting claude swept the variant in.
    #[test]
    #[serial_test::serial]
    fn test_agents_report_cc_mirror_variant_slice_issue36() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing = EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        {
            // Plain claude session (client "claude"): 100 in / 50 out.
            let claude_dir = source_home
                .path()
                .join(".claude")
                .join("projects")
                .join("myproject");
            std::fs::create_dir_all(&claude_dir).unwrap();
            std::fs::write(
                claude_dir.join("conversation.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_plain","message":{"id":"msg_plain","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#,
            )
            .unwrap();
            // cc-mirror variant (client "cc-mirror/kimi-code"): 300 in / 70 out.
            let variant_dir = source_home.path().join(".cc-mirror/kimi-code");
            let config_dir = variant_dir.join("config");
            let project_dir = config_dir.join("projects/proj");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "kimi",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                project_dir.join("session.jsonl"),
                r#"{"type":"assistant","timestamp":"2024-12-01T11:00:00.000Z","requestId":"req_variant","message":{"id":"msg_variant","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70}}}"#,
            )
            .unwrap();

            let home = source_home.path().to_str().unwrap().to_string();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let run = |clients: Option<Vec<String>>| {
                let report = rt
                    .block_on(get_agents_report(ReportOptions {
                        home_dir: Some(home.clone()),
                        use_env_roots: false,
                        clients,
                        ..Default::default()
                    }))
                    .unwrap();
                let input: i64 = report.entries.iter().map(|e| e.input).sum();
                (report.total_messages, input)
            };

            // All clients: both messages fold into "Main".
            assert_eq!(run(None), (2, 400), "all: plain(100) + variant(300)");

            // Variant slice: the claude lane is scanned (so the variant is
            // found), then narrowed to exactly the variant id.
            assert_eq!(
                run(Some(vec!["cc-mirror/kimi-code".to_string()])),
                (1, 300),
                "variant slice = just the variant (300), not empty"
            );

            // Claude slice: plain claude ONLY — the distinct variant is excluded.
            assert_eq!(
                run(Some(vec!["claude".to_string()])),
                (1, 100),
                "claude slice = plain claude (100), not the mixed 400"
            );
        }
    }

    // Agent bucketing + fold arithmetic in isolation (no fixtures): normalized
    // names, the "Main" fallback, plain `+=` token sums, and message_count.max(0).
    #[test]
    fn test_agent_bucket_key_and_accumulator() {
        let msg = |agent: Option<&str>| {
            let mut m = UnifiedMessage::new_with_agent(
                "codebuff",
                "m",
                "p",
                "s",
                0,
                TokenBreakdown {
                    input: 10,
                    output: 5,
                    cache_read: 2,
                    cache_write: 1,
                    reasoning: 3,
                    cache_write_1h: 0,
                },
                0.5,
                agent.map(|a| a.to_string()),
            );
            m.message_count = 2;
            m
        };

        assert_eq!(agent_bucket_key(&msg(None)), "Main");
        assert_eq!(agent_bucket_key(&msg(Some("   "))), "Main");
        assert_eq!(agent_bucket_key(&msg(Some("OmO"))), "Sisyphus");

        let mut acc = AgentAccumulator::default();
        acc.add(&msg(None));
        let mut negative = msg(None);
        negative.message_count = -3; // .max(0) clamp → contributes 0 messages
        acc.add(&negative);

        assert_eq!(acc.input, 20);
        assert_eq!(acc.output, 10);
        assert_eq!(acc.cache_read, 4);
        assert_eq!(acc.cache_write, 2);
        assert_eq!(acc.reasoning, 6);
        assert!((acc.cost - 1.0).abs() < 1e-9);
        assert_eq!(acc.messages, 2, "message_count.max(0): 2 + 0");
        assert!(acc.clients.contains("codebuff"));
    }

    #[test]
    fn agent_accumulator_saturates_overflowing_token_folds() {
        // Vendor-local sibling sweep alongside #823: AgentAccumulator::add is
        // its own per-field CROSS-MESSAGE fold (agents streaming report), not
        // one of the 6 sites #823 covers. An antigravity-cli row can carry an
        // i64::MAX bucket after the untrusted-varint clamp, so two such rows
        // folded into one agent bucket with plain `+=` overflow (debug panic /
        // release wrap) before any saturating grand total runs.
        let make = || {
            UnifiedMessage::new(
                "antigravity-cli",
                "gemini-3-pro",
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
            )
        };

        let mut acc = AgentAccumulator::default();
        acc.add(&make());
        acc.add(&make());

        assert_eq!(acc.input, i64::MAX);
        assert_eq!(acc.cache_read, i64::MAX);
    }

    #[test]
    fn test_agent_bucket_key_copilot_uses_copilot_normalizer() {
        // #724/#751: copilot messages carry a raw OTEL agent id. Our agents
        // report must prettify it with the copilot-specific normalizer (the
        // prettification upstream does in its CLI), while other clients keep the
        // generic normalization.
        let msg = |client: &str, agent: &str| {
            UnifiedMessage::new_with_agent(
                client,
                "m",
                "p",
                "s",
                0,
                TokenBreakdown::default(),
                0.0,
                Some(agent.to_string()),
            )
        };

        // Copilot: raw OTEL ids resolve to their pretty display form.
        assert_eq!(
            agent_bucket_key(&msg("copilot", "github.copilot.default")),
            "GitHub Copilot"
        );
        assert_eq!(
            agent_bucket_key(&msg("copilot", "Plugin:code-review-team:api-reviewer")),
            "Code Review Team: API Reviewer"
        );

        // A non-copilot client with the same raw id must NOT get the
        // copilot-specific prettification (proves the branch is client-scoped).
        assert_ne!(
            agent_bucket_key(&msg("codebuff", "github.copilot.default")),
            "GitHub Copilot"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_refreshes_stale_date_on_cache_hit() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let message_dir = scanner_fixture_path(
                source_home.path(),
                ".local/share/opencode/storage/message/project-1",
            );
            std::fs::create_dir_all(&message_dir).unwrap();
            let path = message_dir.join("msg_001.json");
            std::fs::write(
                &path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();

            let fingerprint = message_cache::SourceFingerprint::from_path(&path).unwrap();
            let mut stale_message = UnifiedMessage::new(
                "opencode",
                "accounts/fireworks/models/deepseek-v3-0324",
                "fireworks",
                "session-1",
                1_733_011_200_000,
                TokenBreakdown {
                    input: 10,
                    output: 5,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
            );
            stale_message.date = "1900-01-01".to_string();

            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                message_cache::CacheIdentity::for_client(ClientId::OpenCode),
                &path,
                fingerprint,
                vec![stale_message],
                Vec::new(),
                None,
            ));
            cache.save_if_dirty();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );

            assert_eq!(messages.len(), 1);
            assert_ne!(messages[0].date, "1900-01-01");
            assert_eq!(
                messages[0].date,
                UnifiedMessage::new(
                    "opencode",
                    "accounts/fireworks/models/deepseek-v3-0324",
                    "fireworks",
                    "session-1",
                    1_733_011_200_000,
                    TokenBreakdown {
                        input: 10,
                        output: 5,
                        cache_read: 0,
                        cache_write: 0,
                        reasoning: 0,
                        cache_write_1h: 0,
                    },
                    0.0,
                )
                .date
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_empty_parse_results_are_not_cached_for_optional_file_sources() {
        use std::os::unix::fs::PermissionsExt;

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let message_dir = scanner_fixture_path(
                source_home.path(),
                ".local/share/opencode/storage/message/project-1",
            );
            std::fs::create_dir_all(&message_dir).unwrap();
            let path = message_dir.join("msg_001.json");
            std::fs::write(
                &path,
                r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            )
            .unwrap();

            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o000);
            std::fs::set_permissions(&path, permissions).unwrap();

            let result = super::collect_local_unified_messages(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
                true,
                &scanner::ScannerSettings::default(),
                None,
            );
            assert!(
                result.is_err(),
                "unreadable sources must not publish an empty generation"
            );

            let cache = message_cache::SourceMessageCache::load();
            assert!(cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::OpenCode),
                    &path
                )
                .is_none());

            let mut readable_permissions = std::fs::metadata(&path).unwrap().permissions();
            readable_permissions.set_mode(0o644);
            std::fs::set_permissions(&path, readable_permissions).unwrap();

            let second_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(second_messages.len(), 1);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_opencode_aliases_preserve_cross_store_identity_across_lanes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        let db_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("opencode-next.db");
        let conn = create_opencode_v2_sqlite_db(&db_path);
        let v1_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.0,
        );
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["legacy-message", "session-overlap", &v1_payload],
        )
        .unwrap();
        let v2_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.02,
        )
        .replacen('{', r#"{"id":"v2-embedded","#, 1);
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["v2-message", "session-overlap", "assistant", &v2_payload],
        )
        .unwrap();
        let deferred_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            20,
            5,
            0,
            0,
            0,
            0.0,
        );
        let deferred_v2_payload = deferred_payload.replacen('{', r#"{"id":"shared-order","#, 1);
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["legacy-order", "session-order", &deferred_payload],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                "v2-order",
                "session-order",
                "assistant",
                &deferred_v2_payload
            ],
        )
        .unwrap();
        drop(conn);

        let sibling_db = db_dir.join("opencode-stable.db");
        let sibling_conn = create_opencode_v2_sqlite_db(&sibling_db);
        sibling_conn
            .execute(
                "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["v2-sibling", "session-overlap", "assistant", &v2_payload],
            )
            .unwrap();
        let provider_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            10,
            5,
            0,
            0,
            0,
            0.02,
        )
        .replacen('{', r#"{"id":"shared-order","#, 1);
        sibling_conn
            .execute(
                "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "v2-order-provider",
                    "session-order",
                    "assistant",
                    &provider_payload
                ],
            )
            .unwrap();
        drop(sibling_conn);

        let json_dir = db_dir.join("storage/message/project-overlap");
        std::fs::create_dir_all(&json_dir).unwrap();
        let migrated_json_payload = build_opencode_sqlite_payload(
            1_700_000_000_000.0,
            1_700_000_000_500.0,
            100,
            10,
            1,
            5,
            2,
            0.03,
        );
        std::fs::write(json_dir.join("legacy-message.json"), migrated_json_payload).unwrap();
        let json_payload = build_opencode_sqlite_payload(
            1_700_000_010_000.0,
            1_700_000_010_500.0,
            30,
            5,
            0,
            0,
            0,
            0.03,
        );
        std::fs::write(json_dir.join("legacy-order.json"), json_payload).unwrap();

        let clients = vec!["opencode".to_string()];
        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(materialized.len(), 3);
        let mut materialized_inputs: Vec<_> = materialized
            .iter()
            .map(|message| message.tokens.input)
            .collect();
        materialized_inputs.sort_unstable();
        assert_eq!(materialized_inputs, vec![10, 20, 100]);
        let migrated = materialized
            .iter()
            .find(|message| message.tokens.input == 100)
            .unwrap();
        assert_eq!(migrated.dedup_key.as_deref(), Some("v2-embedded"));
        assert_eq!(migrated.dedup_aliases, vec!["legacy-message"]);
        assert_eq!(migrated.cost, 0.02);
        assert_eq!(migrated.cost_source, CostSource::ProviderReported);
        let warm = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(
            warm, materialized,
            "cache hits must retain OpenCode aliases"
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 3);
        let mut streamed_inputs: Vec<_> = streamed
            .iter()
            .map(|message| message.tokens.input)
            .collect();
        streamed_inputs.sort_unstable();
        assert_eq!(streamed_inputs, vec![10, 20, 100]);
        let migrated = streamed
            .iter()
            .find(|message| message.tokens.input == 100)
            .unwrap();
        assert_eq!(migrated.dedup_key.as_deref(), Some("v2-embedded"));
        assert_eq!(migrated.dedup_aliases, vec!["legacy-message"]);
        assert_eq!(migrated.cost, 0.02);
        assert_eq!(migrated.cost_source, CostSource::ProviderReported);

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::OpenCode), 3);
        assert_eq!(counted.messages.len(), 3);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            130
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_hybrid_opencode_store_preserves_v1_v2_rows_across_lanes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let _pricing_env =
            EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        let db_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("opencode-next.db");
        let conn = create_opencode_v2_sqlite_db(&db_path);
        conn.execute(
            "INSERT INTO session (id, directory) VALUES (?1, ?2), (?3, ?4)",
            rusqlite::params!["session-v1", "/workspace/v1", "session-v2", "/workspace/v2"],
        )
        .unwrap();
        let v1_data = r#"{
            "id": "message-v1",
            "role": "assistant",
            "modelID": "claude-sonnet-4",
            "providerID": "anthropic",
            "mode": "build",
            "cost": 0.0,
            "tokens": { "input": 100, "output": 10, "reasoning": 1, "cache": { "read": 5, "write": 2 } },
            "time": { "created": 1700000000000.0, "completed": 1700000000500.0 }
        }"#;
        let v2_data = r#"{
            "id": "message-v1",
            "model": { "id": "claude-sonnet-4", "providerID": "anthropic" },
            "agent": "build",
            "cost": 0.02,
            "tokens": { "input": 200, "output": 20, "reasoning": 2, "cache": { "read": 10, "write": 4 } },
            "time": { "created": 1700000001000.0, "completed": 1700000001750.0 }
        }"#;
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params!["row-v1", "session-v1", v1_data],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["row-v2", "session-v2", "assistant", v2_data],
        )
        .unwrap();
        drop(conn);

        let json_dir = db_dir.join("storage/message/project-v1");
        std::fs::create_dir_all(&json_dir).unwrap();
        std::fs::write(
            json_dir.join("message-v1.json"),
            r#"{
                "id": "message-v1",
                "sessionID": "session-v1",
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "mode": "build",
                "cost": 0.01,
                "tokens": { "input": 100, "output": 10, "reasoning": 1, "cache": { "read": 5, "write": 2 } },
                "time": { "created": 1700000000000.0, "completed": 1700000000500.0 }
            }"#,
        )
        .unwrap();

        let clients = vec!["opencode".to_string()];
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        };
        let mut cold = parse_materialized();
        let mut warm = parse_materialized();
        cold.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        warm.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        assert_eq!(cold.len(), 2);
        assert_eq!(
            cold.iter()
                .map(|message| message.dedup_key.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("message-v1"), Some("message-v1")],
            "same embedded ids with incompatible timestamps and tokens must remain distinct"
        );
        assert_eq!(
            cold.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            warm.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            "the rebuilt current-schema entry must preserve v1+v2 output on a warm hit"
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        streamed.sort_by(|left, right| {
            (&left.dedup_key, left.tokens.input).cmp(&(&right.dedup_key, right.tokens.input))
        });
        assert_eq!(
            streamed
                .iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>(),
            cold.iter()
                .map(|message| (&message.dedup_key, &message.tokens))
                .collect::<Vec<_>>()
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::OpenCode), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            300
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        };
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        assert_eq!(model.total_messages, 2);
        assert_eq!(
            (
                model.total_input,
                model.total_output,
                model.total_cache_read,
                model.total_cache_write,
                model
                    .entries
                    .iter()
                    .map(|entry| entry.reasoning)
                    .sum::<i64>(),
            ),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            monthly.entries.iter().fold((0, 0, 0, 0), |totals, entry| (
                totals.0 + entry.input,
                totals.1 + entry.output,
                totals.2 + entry.cache_read,
                totals.3 + entry.cache_write,
            ),),
            (300, 30, 15, 6)
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .fold((0, 0, 0, 0, 0), |totals, entry| (
                    totals.0 + entry.input,
                    totals.1 + entry.output,
                    totals.2 + entry.cache_read,
                    totals.3 + entry.cache_write,
                    totals.4 + entry.reasoning,
                ),),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            agents
                .entries
                .iter()
                .fold((0, 0, 0, 0, 0), |totals, entry| (
                    totals.0 + entry.input,
                    totals.1 + entry.output,
                    totals.2 + entry.cache_read,
                    totals.3 + entry.cache_write,
                    totals.4 + entry.reasoning,
                ),),
            (300, 30, 15, 6, 3)
        );
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            model.total_messages
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            model.total_messages
        );
        assert_eq!(agents.total_messages, model.total_messages);
    }

    #[test]
    #[serial_test::serial]
    fn m16_schema_30_jcode_cache_rebuilds_start_anchor_across_lanes() {
        #[derive(serde::Serialize)]
        struct Schema30Store {
            schema_version: u32,
            entries: Vec<message_cache::CachedSourceEntry>,
        }

        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let _pricing_env =
            EnvGuard::set(&[("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1"))]);

        let sessions_dir = source_home.path().join(".jcode/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let source_path = sessions_dir.join("session_m16.json");
        std::fs::write(
            &source_path,
            r#"{"id":"session_m16","provider_key":"cliproxyapi","model":"claude-sonnet-4","working_dir":"/workspace/m16","messages":[{"id":"u1","role":"user","timestamp":"2026-06-16T12:00:00Z"},{"id":"a1","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":1200,"output_tokens":300},"tool_duration_ms":1000}]}"#,
        )
        .unwrap();

        let start = sessions::utils::parse_timestamp_str("2026-06-16T12:00:00Z").unwrap();
        let end = sessions::utils::parse_timestamp_str("2026-06-16T12:00:01Z").unwrap();
        let fingerprint = message_cache::SourceFingerprint::from_jcode_path(&source_path).unwrap();
        let mut stale_message = UnifiedMessage::new_with_dedup(
            "jcode",
            "claude-sonnet-4",
            "cliproxyapi",
            "session_m16",
            end,
            TokenBreakdown {
                input: 1200,
                output: 300,
                ..Default::default()
            },
            0.0,
            Some("stale-schema-30".to_string()),
        );
        stale_message.duration_ms = Some(end - start);
        let stale_store = Schema30Store {
            schema_version: 30,
            entries: vec![message_cache::CachedSourceEntry::new(
                message_cache::CacheIdentity::for_client(ClientId::Jcode),
                &source_path,
                fingerprint.clone(),
                vec![stale_message],
                Vec::new(),
                None,
            )],
        };
        let cache_file = crate::paths::get_cache_dir().join("source-message-cache.bin");
        std::fs::create_dir_all(cache_file.parent().unwrap()).unwrap();
        let writer = std::io::BufWriter::new(std::fs::File::create(&cache_file).unwrap());
        bincode::options()
            .serialize_into(writer, &stale_store)
            .unwrap();

        assert_eq!(
            message_cache::SourceFingerprint::from_jcode_path(&source_path).unwrap(),
            fingerprint,
            "the source fingerprint must stay unchanged across the schema-only rebuild"
        );
        // The schema-30 shard stays on disk as the upgrade input; the cold
        // scan below must rebuild the corrected start anchor from source.

        let clients = vec!["jcode".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_source_path = scanner_spelling(scan_result.get(ClientId::Jcode), &source_path);
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        };
        let cold = parse_materialized();
        let warm = parse_materialized();
        assert_eq!(cold.len(), 1);
        assert_eq!(
            (
                cold[0].timestamp,
                cold[0].duration_ms,
                cold[0].tokens.input,
                cold[0].tokens.output,
            ),
            (start, Some(end - start), 1200, 300)
        );
        assert_eq!(
            warm.iter()
                .map(|message| (
                    message.timestamp,
                    message.duration_ms,
                    message.tokens.clone()
                ))
                .collect::<Vec<_>>(),
            cold.iter()
                .map(|message| (
                    message.timestamp,
                    message.duration_ms,
                    message.tokens.clone()
                ))
                .collect::<Vec<_>>()
        );
        let rebuilt_cache = message_cache::SourceMessageCache::load();
        assert_eq!(
            rebuilt_cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Jcode),
                    &cache_source_path
                )
                .unwrap()
                .messages
                .len(),
            1
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(
            (
                streamed[0].timestamp,
                streamed[0].duration_ms,
                streamed[0].tokens.input,
                streamed[0].tokens.output,
            ),
            (start, Some(end - start), 1200, 300)
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Jcode), 1);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(
            (counted.messages[0].input, counted.messages[0].output),
            (1200, 300)
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients),
            ..Default::default()
        };
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        assert_eq!(
            (model.total_messages, model.total_input, model.total_output),
            (1, 1200, 300)
        );
        assert_eq!(
            monthly.entries.iter().fold((0, 0, 0), |totals, entry| (
                totals.0 + entry.message_count,
                totals.1 + entry.input,
                totals.2 + entry.output,
            )),
            (1, 1200, 300)
        );
        assert_eq!(
            hourly.entries.iter().fold((0, 0, 0), |totals, entry| (
                totals.0 + entry.message_count,
                totals.1 + entry.input,
                totals.2 + entry.output,
            )),
            (1, 1200, 300)
        );
        assert_eq!(agents.total_messages, 1);
        assert_eq!(
            agents.entries.iter().fold((0, 0), |totals, entry| (
                totals.0 + entry.input,
                totals.1 + entry.output,
            )),
            (1200, 300)
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_sqlite_source_cache_invalidates_on_wal_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");

            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let journal_mode: String = conn
                .query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(journal_mode.to_lowercase(), "wal");
            conn.execute_batch(
                "PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE message (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL,
                     data TEXT NOT NULL
                 );",
            )
            .unwrap();

            let row_one = r#"{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "tokens": { "input": 100, "output": 50, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "time": { "created": 1700000000000.0 }
            }"#;
            let row_two = r#"{
                "role": "assistant",
                "modelID": "claude-sonnet-4",
                "providerID": "anthropic",
                "tokens": { "input": 120, "output": 60, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                "time": { "created": 1700000001000.0 }
            }"#;

            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["msg-1", "session-1", row_one],
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);

            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params!["msg-2", "session-1", row_two],
            )
            .unwrap();
            assert!(db_path.with_extension("db-wal").exists());

            let refreshed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(refreshed_messages.len(), 2);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_dedups_across_channel_suffixed_opencode_dbs() {
        // Regression guard: a session that appears in both `opencode.db` and
        // `opencode-<channel>.db` (e.g. the user switches channels mid-session)
        // must only be counted once.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();

            let schema = "PRAGMA journal_mode=WAL;
                 PRAGMA wal_autocheckpoint=0;
                 CREATE TABLE message (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL,
                     data TEXT NOT NULL
                 );";
            let row = |input: u64, ts: u64| {
                format!(
                    r#"{{
                        "role": "assistant",
                        "modelID": "claude-sonnet-4",
                        "providerID": "anthropic",
                        "tokens": {{ "input": {input}, "output": 10, "reasoning": 0, "cache": {{ "read": 0, "write": 0 }} }},
                        "time": {{ "created": {ts}.0 }}
                    }}"#
                )
            };

            let default_db = db_dir.join("opencode.db");
            let conn = rusqlite::Connection::open(&default_db).unwrap();
            conn.execute_batch(schema).unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "shared-msg",
                    "session-shared",
                    row(100, 1_700_000_000_000u64)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "latest-only",
                    "session-latest",
                    row(200, 1_700_000_001_000u64)
                ],
            )
            .unwrap();
            drop(conn);

            let stable_db = db_dir.join("opencode-stable.db");
            let conn = rusqlite::Connection::open(&stable_db).unwrap();
            conn.execute_batch(schema).unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "shared-msg",
                    "session-shared",
                    row(100, 1_700_000_000_000u64)
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![
                    "stable-only",
                    "session-stable",
                    row(300, 1_700_000_002_000u64)
                ],
            )
            .unwrap();
            drop(conn);

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(
                messages.len(),
                3,
                "expected 3 unique messages (shared + latest-only + stable-only), got {}",
                messages.len()
            );
            let mut ids: Vec<String> = messages
                .iter()
                .filter_map(|m| m.dedup_key.clone())
                .collect();
            ids.sort();
            assert_eq!(ids, vec!["latest-only", "shared-msg", "stable-only"]);

            let messages_warm = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );
            assert_eq!(
                messages_warm.len(),
                3,
                "warm cache must also dedup shared message across channel dbs"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_opencode_sqlite_deduplicates_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");
            let conn = create_opencode_sqlite_db(&db_path);

            let msg_a = build_opencode_sqlite_payload(
                1_700_000_000_000.0,
                1_700_000_000_500.0,
                100,
                50,
                0,
                10,
                5,
                0.01,
            );
            let msg_b = build_opencode_sqlite_payload(
                1_700_000_001_000.0,
                1_700_000_001_500.0,
                200,
                80,
                10,
                20,
                0,
                0.02,
            );
            let msg_c = build_opencode_sqlite_payload(
                1_700_000_002_000.0,
                1_700_000_002_500.0,
                300,
                120,
                15,
                0,
                0,
                0.03,
            );

            for (id, session_id, payload) in [
                ("root_a", "root", msg_a.as_str()),
                ("root_b", "root", msg_b.as_str()),
                ("fork_a_copy", "fork", msg_a.as_str()),
                ("fork_b_copy", "fork", msg_b.as_str()),
                ("fork_c_new", "fork", msg_c.as_str()),
            ] {
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, session_id, payload],
                )
                .unwrap();
            }
            drop(conn);

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["opencode".to_string()],
                None,
            );

            assert_eq!(messages.len(), 3);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 600);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 250);
            assert_eq!(messages.iter().map(|m| m.cost).sum::<f64>(), 0.06);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_opencode_sqlite_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        {
            let db_dir = source_home.path().join(".local/share/opencode");
            std::fs::create_dir_all(&db_dir).unwrap();
            let db_path = db_dir.join("opencode.db");
            let conn = create_opencode_sqlite_db(&db_path);

            let msg_a = build_opencode_sqlite_payload(
                1_700_000_000_000.0,
                1_700_000_000_500.0,
                100,
                50,
                0,
                10,
                5,
                0.01,
            );
            let msg_b = build_opencode_sqlite_payload(
                1_700_000_001_000.0,
                1_700_000_001_500.0,
                200,
                80,
                10,
                20,
                0,
                0.02,
            );
            let msg_c = build_opencode_sqlite_payload(
                1_700_000_002_000.0,
                1_700_000_002_500.0,
                300,
                120,
                15,
                0,
                0,
                0.03,
            );

            for (id, session_id, payload) in [
                ("root_a", "root", msg_a.as_str()),
                ("root_b", "root", msg_b.as_str()),
                ("fork_a_copy", "fork", msg_a.as_str()),
                ("fork_b_copy", "fork", msg_b.as_str()),
                ("fork_c_new", "fork", msg_c.as_str()),
            ] {
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![id, session_id, payload],
                )
                .unwrap();
            }
            drop(conn);

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["opencode".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::OpenCode), 3);
            assert_eq!(parsed.messages.len(), 3);
            assert_eq!(parsed.messages.iter().map(|m| m.input).sum::<i64>(), 600);
            assert_eq!(parsed.messages.iter().map(|m| m.output).sum::<i64>(), 250);
        }
    }

    /// Regression fixture for Codex sessions that are live-only, archive-only,
    /// or briefly present in both roots while the CLI moves a transcript.
    fn write_codex_sessions_and_archived_sessions_fixture(source_home: &std::path::Path) {
        let sessions_dir = source_home.join(".codex/sessions");
        let archived_dir = source_home.join(".codex/archived_sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();

        std::fs::write(
            sessions_dir.join("live-only.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-25T10:00:00Z","type":"session_meta","payload":{"id":"33333333-3333-7333-8333-333333333333","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"output_tokens":5,"total_tokens":55},"last_token_usage":{"input_tokens":50,"output_tokens":5,"total_tokens":55}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        std::fs::write(
            archived_dir.join("archived-only.jsonl"),
            concat!(
                r#"{"timestamp":"2026-06-20T09:00:00Z","type":"session_meta","payload":{"id":"44444444-4444-7444-8444-444444444444","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-20T09:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-20T09:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":70,"output_tokens":7,"total_tokens":77},"last_token_usage":{"input_tokens":70,"output_tokens":7,"total_tokens":77}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let shared_content = concat!(
            r#"{"timestamp":"2026-06-22T08:00:00Z","type":"session_meta","payload":{"id":"55555555-5555-7555-8555-555555555555","source":"interactive","model_provider":"openai","cwd":"/repo"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-22T08:00:01Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-22T08:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33},"last_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}"#,
            "\n"
        );
        std::fs::write(
            sessions_dir.join("shared-in-sessions.jsonl"),
            shared_content,
        )
        .unwrap();
        std::fs::write(
            archived_dir.join("shared-in-archived.jsonl"),
            shared_content,
        )
        .unwrap();
    }

    fn scan_test_sources(
        source_home: &Path,
        clients: &[String],
        scanner_settings: &scanner::ScannerSettings,
    ) -> scanner::ScanResult {
        scanner::scan_all_clients_with_scanner_settings(
            source_home.to_str().unwrap(),
            clients,
            false,
            scanner_settings,
        )
    }

    // Cache keys preserve the scanner-returned path spelling. On Windows an
    // explicit-home resolver can use `/` while TempDir::join produces `\`, so
    // cache fixtures must seed and query with the production scanner spelling.
    fn scanner_spelling(paths: &[PathBuf], expected: &Path) -> PathBuf {
        let expected = std::fs::canonicalize(expected).unwrap();
        paths
            .iter()
            .find(|path| std::fs::canonicalize(path).is_ok_and(|path| path == expected))
            .cloned()
            .expect("scanner must discover the fixture path")
    }
    fn with_isolated_tokscale_cache<T>(
        cache_home: &std::path::Path,
        action: impl FnOnce() -> T,
    ) -> T {
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
            ("SYRTIS_USAGE_DIR", cache_home.join("usage").as_os_str()),
            (
                "SYRTIS_CONFIG_DIR",
                cache_home.join("syrtis-config").as_os_str(),
            ),
            ("XDG_DATA_HOME", cache_home.join(".local/share").as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        action()
    }

    fn store_cache_has_entries(cache_home: &std::path::Path, namespace: &str) -> bool {
        let database = rusqlite::Connection::open_with_flags(
            cache_home.join("usage/source-message-cache.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        database.query_row(
            "SELECT EXISTS(SELECT 1 FROM mc_sources s JOIN mc_versions v ON v.source_id=s.id AND v.generation=s.generation WHERE s.namespace=?1 AND s.active=1 AND v.cacheable=1)",
            [namespace], |row| row.get(0),
        ).unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn test_claude_shipping_report_cold_start_persists_store() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let transcripts_dir = source_home.path().join(".claude/transcripts");
        std::fs::create_dir_all(&transcripts_dir).unwrap();
        let source = transcripts_dir.join("ses_123456789012345678901234567.jsonl");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"user","timestamp":"2026-04-01T10:00:00.000Z","message":{"content":"prompt"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-04-01T10:00:01.000Z","requestId":"req-shipping","message":{"id":"msg-shipping","model":"claude-sonnet-4","usage":{"input_tokens":123,"output_tokens":45}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || ReportOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            ..Default::default()
        };
        let cold = with_isolated_tokscale_cache(cache_home.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        assert_eq!(cold.total_messages, 1);
        assert!(store_cache_has_entries(cache_home.path(), "claude"));
        let clients = ["claude".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_source = scanner_spelling(scan_result.get(ClientId::Claude), &source);
        with_isolated_tokscale_cache(cache_home.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            assert!(cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Claude)
                        .with_scope(source_home.path()),
                    &cache_source,
                )
                .is_some_and(|entry| entry.messages.len() == 1));
        });

        let warm = with_isolated_tokscale_cache(cache_home.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        assert_eq!(warm.total_messages, cold.total_messages);
        assert_eq!(warm.total_input, cold.total_input);
        assert_eq!(warm.total_output, cold.total_output);
    }

    #[test]
    #[serial_test::serial]
    fn test_claude_shipping_warm_cache_reuses_persisted_parent_paths() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let project_dir = source_home.path().join(".claude/projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let source = project_dir.join("agent-child.jsonl");
        std::fs::write(
            &source,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"sessionId":"flat-parent","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-source","message":{"id":"msg-source","model":"claude-sonnet-4","usage":{"input_tokens":123,"output_tokens":45}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let clients = ["claude".to_string()];
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_source = scanner_spelling(scan_result.get(ClientId::Claude), &source);

        let mut fingerprint = message_cache::SourceFingerprint::from_claude_code_path_with_home(
            &cache_source,
            Some(source_home.path()),
        )
        .unwrap();
        let persisted_parent = fingerprint
            .related_files
            .iter_mut()
            .find(|related| related.suffix.starts_with("parent-session-"))
            .unwrap();
        persisted_parent.path =
            message_cache::CachedPath::from_path(&project_dir.join("cached-only-parent.jsonl"));
        assert!(!persisted_parent.exists);

        let sentinel = UnifiedMessage::new_with_dedup(
            "claude",
            "cached-sentinel",
            "anthropic",
            "cached-session",
            1_767_225_600_000,
            TokenBreakdown {
                input: 999,
                output: 1,
                ..Default::default()
            },
            0.0,
            Some("cached-sentinel".to_string()),
        );
        with_isolated_tokscale_cache(cache_home.path(), || {
            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                message_cache::CacheIdentity::for_client(ClientId::Claude)
                    .with_scope(source_home.path()),
                &cache_source,
                fingerprint,
                vec![sentinel],
                Vec::new(),
                None,
            ));
            cache.save_if_dirty();
        });

        let mut streamed = Vec::new();
        with_isolated_tokscale_cache(cache_home.path(), || {
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |message| streamed.push(message.clone()),
            );
        });

        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].model_id, "cached-sentinel");
        assert_eq!(streamed[0].tokens.input, 999);
    }

    #[test]
    #[serial_test::serial]
    fn test_zed_shipping_report_cold_start_persists_store() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let threads_dir = source_home.path().join("custom-zed/threads");
        std::fs::create_dir_all(&threads_dir).unwrap();
        let source = threads_dir.join("threads.db");
        let connection = create_zed_sqlite_db(&source);
        insert_zed_thread(&connection, "zed-shipping", "claude-sonnet-4-5");
        drop(connection);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || {
            let mut extra_scan_paths = std::collections::BTreeMap::new();
            extra_scan_paths.insert("zed".to_string(), vec![threads_dir.clone()]);
            ReportOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["zed".to_string()]),
                scanner_settings: scanner::ScannerSettings {
                    extra_scan_paths,
                    ..Default::default()
                },
                ..Default::default()
            }
        };
        let cold = with_isolated_tokscale_cache(cache_home.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        assert_eq!(cold.total_messages, 1);
        assert!(store_cache_has_entries(cache_home.path(), "zed"));
        with_isolated_tokscale_cache(cache_home.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            assert!(cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Zed),
                    &source,
                )
                .is_some_and(|entry| entry.messages.len() == 1));
        });

        let warm = with_isolated_tokscale_cache(cache_home.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        assert_eq!(warm.total_messages, cold.total_messages);
        assert_eq!(warm.total_input, cold.total_input);
        assert_eq!(warm.total_output, cold.total_output);
    }

    fn write_codex_duration_prefix_fixture(
        source_home: &std::path::Path,
    ) -> (std::path::PathBuf, String) {
        let lines = include_str!("../tests/fixtures/codex_duration_timing.jsonl")
            .lines()
            .collect::<Vec<_>>();
        let sessions_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let source = sessions_dir.join("codex-duration-timing.jsonl");
        std::fs::write(&source, format!("{}\n", lines[..5].join("\n"))).unwrap();
        (source, format!("{}\n", lines[5..].join("\n")))
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_non_overlapping_durations_survive_incremental_and_streaming_caches() {
        let materialized_source_home = tempfile::TempDir::new().unwrap();
        let streaming_source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let (source, suffix) = write_codex_duration_prefix_fixture(materialized_source_home.path());
        let (streaming_source, streaming_suffix) =
            write_codex_duration_prefix_fixture(streaming_source_home.path());

        let home = materialized_source_home
            .path()
            .to_str()
            .unwrap()
            .to_string();
        let streaming_home = streaming_source_home.path().to_str().unwrap().to_string();
        let clients = vec!["codex".to_string()];
        let streaming_scan = scan_test_sources(
            streaming_source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let cache_streaming_source =
            scanner_spelling(streaming_scan.get(ClientId::Codex), &streaming_source);
        let parse_materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        };

        let prefix = with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].duration_ms, Some(1_000));

        let mut source_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap();
        source_file.write_all(suffix.as_bytes()).unwrap();
        source_file.flush().unwrap();
        drop(source_file);

        let materialized_incremental =
            with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        let materialized_warm =
            with_isolated_tokscale_cache(materialized_cache.path(), parse_materialized);
        for (phase, messages) in [
            ("incremental", &materialized_incremental),
            ("warm", &materialized_warm),
        ] {
            assert_eq!(messages.len(), 3, "{phase} materialized messages");
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.duration_ms)
                    .collect::<Vec<_>>(),
                vec![Some(1_000), Some(4_000), Some(2_000)],
                "{phase} materialized durations"
            );
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || ReportOptions {
            home_dir: Some(streaming_home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        };
        let streaming_cold = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        assert_eq!(streaming_cold.total_messages, 1);
        assert!(
            store_cache_has_entries(streaming_cache.path(), "codex"),
            "the cold shipping report must persist its own Codex source"
        );
        with_isolated_tokscale_cache(streaming_cache.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            let incremental = cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &cache_streaming_source,
                )
                .and_then(|entry| entry.codex_incremental)
                .expect("cold shipping report must persist Codex incremental state");
            assert_eq!(
                incremental.consumed_offset,
                std::fs::metadata(&streaming_source).unwrap().len()
            );
        });

        let mut streaming_source_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&streaming_source)
            .unwrap();
        streaming_source_file
            .write_all(streaming_suffix.as_bytes())
            .unwrap();
        streaming_source_file.flush().unwrap();
        drop(streaming_source_file);

        let streaming_incremental = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        with_isolated_tokscale_cache(streaming_cache.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            let incremental = cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &cache_streaming_source,
                )
                .and_then(|entry| entry.codex_incremental)
                .expect("incremental shipping report must refresh Codex state");
            assert_eq!(
                incremental.consumed_offset,
                std::fs::metadata(&streaming_source).unwrap().len()
            );
        });
        let streaming_warm = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });

        for (phase, report) in [
            ("incremental", &streaming_incremental),
            ("warm", &streaming_warm),
        ] {
            assert_eq!(report.total_messages, 3, "{phase} streaming messages");
            assert_eq!(report.entries.len(), 1, "{phase} model groups");
            let performance = &report.entries[0].performance;
            assert_eq!(
                performance.total_duration_ms, 7_000,
                "{phase} total duration"
            );
            assert_eq!(performance.timed_tokens, 160, "{phase} timed tokens");
            assert_eq!(performance.sample_count, 3, "{phase} samples");
            assert_eq!(performance.token_coverage, 1.0, "{phase} coverage");
            let expected_ms_per_1k = 7_000.0 * 1_000.0 / 160.0;
            assert!(
                (performance.ms_per_1k_tokens.unwrap() - expected_ms_per_1k).abs() < f64::EPSILON,
                "{phase} milliseconds per 1K tokens"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_archive_roots_are_exact_once_across_all_consumers() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let count_cache = tempfile::TempDir::new().unwrap();

        write_codex_sessions_and_archived_sessions_fixture(source_home.path());

        let home = source_home.path().to_str().unwrap().to_string();
        let clients = vec!["codex".to_string()];
        let materialized = with_isolated_tokscale_cache(materialized_cache.path(), || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        });

        assert_eq!(materialized.len(), 3);
        let session_ids: HashSet<_> = materialized
            .iter()
            .map(|message| message.session_id.as_str())
            .collect();
        assert!(session_ids.contains("live-only"));
        assert!(session_ids.contains("archived-only"));
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            150,
        );
        assert_eq!(
            materialized
                .iter()
                .map(|message| message.tokens.output)
                .sum::<i64>(),
            15,
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = || ReportOptions {
            home_dir: Some(home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        };
        let streaming_cold = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });

        assert!(
            store_cache_has_entries(streaming_cache.path(), "codex"),
            "the cold shipping report must persist the cache used by the warm pass",
        );
        let streaming_warm = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(report_options()))
                .unwrap()
        });
        for (phase, streaming) in [("cold", &streaming_cold), ("warm", &streaming_warm)] {
            assert_eq!(streaming.total_messages, 3, "{phase} streaming messages");
            assert_eq!(streaming.total_input, 150, "{phase} streaming input");
            assert_eq!(streaming.total_output, 15, "{phase} streaming output");
        }

        let counted = with_isolated_tokscale_cache(count_cache.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap()
        });
        assert_eq!(counted.counts.get(ClientId::Codex), 3);
        assert_eq!(counted.messages.len(), 3);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            150,
        );
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.output)
                .sum::<i64>(),
            15,
        );
    }

    fn write_codex_forked_history_fixture(source_home: &std::path::Path) {
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("parent.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T10:00:00Z","type":"session_meta","payload":{"id":"parent-session","source":"interactive","model_provider":"openai","cwd":"/Users/alice/root"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            codex_dir.join("fork.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T10:01:00Z","type":"session_meta","payload":{"id":"fork-session","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1}}},"model_provider":"openai","cwd":"/Users/alice/root-worktree"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:02Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"total_tokens":130},"last_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"total_tokens":65}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T10:01:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"total_tokens":143},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"total_tokens":13}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    fn write_codex_parent_replay_fixture(source_home: &std::path::Path) {
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("parent.jsonl"),
            concat!(
                r#"{"timestamp":"2026-05-24T20:00:00Z","type":"session_meta","payload":{"id":"019e5b00-0000-7000-8000-000000000001","source":"vscode","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:01Z","type":"turn_context","payload":{"turn_id":"019e5b00-0001-7000-8000-000000000001","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110},"last_token_usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}"#,
                "\n",
                r#"{"timestamp":"2026-05-24T20:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":130,"output_tokens":13,"total_tokens":143},"last_token_usage":{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        for (filename, child_id, child_turn_id, timestamp) in [
            (
                "child-a.jsonl",
                "019e5c03-1e99-7000-8000-000000000001",
                "019e5c03-6425-7000-8000-000000000001",
                "2026-05-24T21:00:00Z",
            ),
            (
                "child-b.jsonl",
                "019e5c04-1e99-7000-8000-000000000001",
                "019e5c04-6425-7000-8000-000000000001",
                "2026-05-24T22:00:00Z",
            ),
        ] {
            std::fs::write(
                codex_dir.join(filename),
                format!(
                    concat!(
                        r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"{child_id}","forked_from_id":"019e5b00-0000-7000-8000-000000000001","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"019e5b00-0000-7000-8000-000000000001","depth":1}}}}}},"model_provider":"openai","agent_nickname":"worker","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"019e5b00-0000-7000-8000-000000000001","source":"vscode","model_provider":"openai","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"turn_context","payload":{{"turn_id":"019e5b00-0001-7000-8000-000000000001","model":"gpt-5.5","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":100,"output_tokens":10,"total_tokens":110}},"last_token_usage":{{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}}}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":130,"output_tokens":13,"total_tokens":143}},"last_token_usage":{{"input_tokens":30,"output_tokens":3,"total_tokens":33}}}}}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"task_started","turn_id":"{child_turn_id}"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"turn_context","payload":{{"turn_id":"{child_turn_id}","model":"gpt-5.5","cwd":"/repo"}}}}"#,
                        "\n",
                        r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":140,"output_tokens":14,"total_tokens":154}},"last_token_usage":{{"input_tokens":10,"output_tokens":1,"total_tokens":11}}}}}}}}"#,
                        "\n",
                    ),
                    timestamp = timestamp,
                    child_id = child_id,
                    child_turn_id = child_turn_id,
                ),
            )
            .unwrap();
        }
    }

    fn write_codex_user_fork_replay_fixture(source_home: &std::path::Path) {
        let sessions_dir = source_home.join(".codex/sessions/2026/01/02");
        let archived_dir = source_home.join(".codex/archived_sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        std::fs::create_dir_all(&archived_dir).unwrap();

        std::fs::write(
            archived_dir.join("rollout-2026-01-02T03-04-05-11111111-1111-7111-8111-111111111111.jsonl"),
            concat!(
                r#"{"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:06Z","type":"turn_context","payload":{"turn_id":"11111111-3333-7333-8333-333333333333","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:07Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:04:08Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":450,"output_tokens":120,"total_tokens":1320},"last_token_usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n"
            ),
        )
        .unwrap();

        std::fs::write(
            sessions_dir.join("rollout-2026-01-02T03-10-00-22222222-2222-7222-8222-222222222222.jsonl"),
            concat!(
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"session_meta","payload":{"id":"22222222-2222-7222-8222-222222222222","forked_from_id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"session_meta","payload":{"id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"turn_context","payload":{"turn_id":"11111111-3333-7333-8333-333333333333","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":100,"total_tokens":1100}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":450,"output_tokens":120,"total_tokens":1320},"last_token_usage":{"input_tokens":200,"cached_input_tokens":50,"output_tokens":20,"total_tokens":220}}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:30Z","type":"turn_context","payload":{"turn_id":"22222222-4444-7444-8444-444444444444","model":"gpt-5.5","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:30Z","type":"session_meta","payload":{"id":"22222222-2222-7222-8222-222222222222","forked_from_id":"11111111-1111-7111-8111-111111111111","source":"vscode","thread_source":"user","model_provider":"openai","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-02T03:10:53Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1500,"cached_input_tokens":500,"output_tokens":150,"total_tokens":1650},"last_token_usage":{"input_tokens":300,"cached_input_tokens":50,"output_tokens":30,"total_tokens":330}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_deduplicates_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_forked_history_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(messages.len(), 3);
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.input)
                    .sum::<i64>(),
                88
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.cache_read)
                    .sum::<i64>(),
                22
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.output)
                    .sum::<i64>(),
                33
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_keeps_user_fork_own_turn() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_user_fork_replay_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            let session_ids: HashSet<_> = messages
                .iter()
                .map(|message| message.session_id.as_str())
                .collect();
            assert!(session_ids
                .contains("rollout-2026-01-02T03-10-00-22222222-2222-7222-8222-222222222222"));
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 1000);
            assert_eq!(
                messages.iter().map(|m| m.tokens.cache_read).sum::<i64>(),
                500
            );
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 150);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_deduplicates_parent_replay_across_forks() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_parent_replay_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            // Parent contributes its two turns. The two forks each replay the
            // parent history (skipped) and then emit one own turn that lands on
            // the identical cumulative total (140/14). Sibling forks sharing a
            // cumulative total is the signature of a replayed row, so the
            // fork-parent-scoped dedup key collapses them into one. Real fork
            // fan-out replays the same upstream totals into 10-100+ siblings;
            // two distinct turns reaching a byte-identical cumulative vector by
            // chance does not happen in practice because the cumulative encodes
            // each fork's divergent context size.
            assert_eq!(messages.len(), 3);
            assert_eq!(messages.iter().map(|m| m.tokens.input).sum::<i64>(), 140);
            assert_eq!(messages.iter().map(|m| m.tokens.output).sum::<i64>(), 14);
        }
    }

    fn write_codex_twin_token_count_fixture(source_home: &std::path::Path) {
        // Single session with two turns whose `last_token_usage` deltas are
        // byte-identical but emitted at different timestamps. The fork-dedup
        // key includes the cumulative total, so both turns must survive even
        // when a user happens to send two turns producing the same per-turn
        // delta.
        let codex_dir = source_home.join(".codex/sessions");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("twin-deltas.jsonl"),
            concat!(
                r#"{"timestamp":"2026-04-30T11:00:00Z","type":"session_meta","payload":{"id":"twin-session","source":"interactive","model_provider":"openai","cwd":"/Users/alice/root"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n",
                r#"{"timestamp":"2026-04-30T11:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":20,"cached_input_tokens":4,"output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_codex_keeps_twin_token_counts_at_distinct_timestamps() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_twin_token_count_fixture(source_home.path());

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(
                messages.len(),
                2,
                "two turns with identical token deltas at distinct timestamps must both survive dedup",
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.input)
                    .sum::<i64>(),
                16,
                "input tokens normalize cache_read out of input: 2 turns × (10 - 2) = 16",
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.output)
                    .sum::<i64>(),
                6,
            );
            assert_eq!(
                messages
                    .iter()
                    .map(|message| message.tokens.cache_read)
                    .sum::<i64>(),
                4,
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_codex_counts_deduplicated_forked_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            write_codex_forked_history_fixture(source_home.path());

            let parsed = parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["codex".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();

            assert_eq!(parsed.counts.get(ClientId::Codex), 3);
            assert_eq!(parsed.messages.len(), 3);
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.input)
                    .sum::<i64>(),
                88
            );
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.cache_read)
                    .sum::<i64>(),
                22
            );
            assert_eq!(
                parsed
                    .messages
                    .iter()
                    .map(|message| message.output)
                    .sum::<i64>(),
                33
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_reparses_from_zero_when_incremental_prefix_is_stale() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert_eq!(initial_messages[0].model_id, "gpt-5.4");

            std::thread::sleep(std::time::Duration::from_millis(5));
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(fresh_messages.len(), 2);
            assert!(fresh_messages
                .iter()
                .all(|message| message.model_id == "gpt-5.5"));
            // Full reparsing must deliver both current rows. The original
            // content-derived model key remains a distinct collected fact.
            let mut expected = fresh_messages;
            expected.extend(initial_messages);
            expected.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            let mut actual = warm_messages;
            actual.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            assert_eq!(actual, expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_keeps_untimestamped_rows_in_sync_after_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_matches_cold_parse_after_malformed_json_append() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let codex_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&codex_dir).unwrap();
            let path = codex_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":999""#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert!(message_cache::SourceMessageCache::load()
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &path
                )
                .is_none());

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_exact_hit_codex_cache_repairs_fallback_timestamps_without_incremental_state() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let expected = crate::sessions::codex::parse_codex_file(&path);
            assert_eq!(expected.len(), 1);

            let fingerprint = message_cache::SourceFingerprint::from_path(&path).unwrap();
            let mut stale_message = expected[0].clone();
            stale_message.timestamp = 0;
            stale_message.date = "1900-01-01".to_string();

            let mut cache = message_cache::SourceMessageCache::default();
            cache.insert(message_cache::CachedSourceEntry::new(
                message_cache::CacheIdentity::for_client(ClientId::Codex),
                &path,
                fingerprint,
                vec![stale_message],
                vec![0],
                None,
            ));
            cache.save_if_dirty();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(messages, expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_repairs_fallback_timestamps_after_source_mtime_change() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            let contents = concat!(
                r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n"
            );
            std::fs::write(&path, contents).unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);

            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(&path, contents).unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_ne!(warm_messages[0].timestamp, initial_messages[0].timestamp);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_full_log_parse_preserves_valid_messages_before_invalid_line_error() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");

            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.write_all(&[0xff, b'\n']).unwrap();
            file.flush().unwrap();

            let messages = sessions::codex::parse_codex_file(&path);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].model_id, "gpt-5.4");
            let context = ResolvedLocalSourceContext::capture(
                Some(source_home.path().to_path_buf()),
                false,
                scanner::ScannerSettings::default(),
            )
            .unwrap();
            assert!(
                super::synchronize_local_usage(
                    &context,
                    LocalParseOptions {
                        clients: Some(vec!["codex".to_string()]),
                        ..Default::default()
                    },
                    super::LocalUsageSelection::LocalClients,
                )
                .is_err(),
                "production synchronization must reject the incomplete source"
            );

            let cache = message_cache::SourceMessageCache::load();
            assert!(cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &path
                )
                .is_none());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_does_not_persist_unknown_before_later_turn_context() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"session_meta","payload":{"source":"interactive","model_provider":"openai"}}"#,
                    "\n",
                    r#"{"timestamp":"2026-04-27T10:00:00Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                    "\n"
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert_eq!(initial_messages[0].model_id, "unknown");
            assert!(message_cache::SourceMessageCache::load()
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &path
                )
                .is_none());

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    r#"{"timestamp":"2026-04-27T10:00:04Z","type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let resumed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(resumed_messages, fresh_messages);
            assert_eq!(resumed_messages.len(), 1);
            assert_eq!(resumed_messages[0].model_id, "gpt-5.5");

            drop(_fresh_env);
            assert!(message_cache::SourceMessageCache::load()
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &path
                )
                .is_some());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_cache_skips_non_newline_terminated_resume_prefix() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let fresh_cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let session_dir = scanner_fixture_path(source_home.path(), ".codex/sessions");
            std::fs::create_dir_all(&session_dir).unwrap();
            let path = session_dir.join("session.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"turn_context","payload":{"model":"gpt-5.4"}}"#,
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#
                ),
            )
            .unwrap();

            let initial_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );
            assert_eq!(initial_messages.len(), 1);
            assert!(message_cache::SourceMessageCache::load()
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &path
                )
                .is_none());

            std::thread::sleep(std::time::Duration::from_millis(5));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(
                concat!(
                    "\n",
                    r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2}}}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
            file.flush().unwrap();

            let warm_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            let _fresh_env = EnvGuard::set(&[
                ("HOME", fresh_cache_home.path().as_os_str()),
                ("TOKSCALE_CONFIG_DIR", fresh_cache_home.path().as_os_str()),
            ]);
            let fresh_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["codex".to_string()],
                None,
            );

            assert_eq!(warm_messages, fresh_messages);
            assert_eq!(warm_messages.len(), 2);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_does_not_reuse_priced_cost_without_pricing_service() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", temp_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", temp_home.path().as_os_str()),
        ]);
        {
            let cursor_cache_dir = source_home.path().join(".config/tokscale/cursor-cache");
            std::fs::create_dir_all(&cursor_cache_dir).unwrap();

            let csv = r#"Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost
"2026-03-04T12:00:00.000Z","Included","Composer 1.5","No","1200","1000","5000","2000","8000","Included""#;
            std::fs::write(cursor_cache_dir.join("usage.csv"), csv).unwrap();

            let mut litellm = HashMap::new();
            litellm.insert(
                "Composer 1.5".into(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.001),
                    output_cost_per_token: Some(0.002),
                    cache_read_input_token_cost: Some(0.0005),
                    ..Default::default()
                },
            );
            let pricing = pricing::PricingService::new(litellm, HashMap::new());

            let repriced_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["cursor".to_string()],
                Some(&pricing),
            );
            assert_eq!(repriced_messages.len(), 1);
            assert!(repriced_messages[0].cost > 0.0);

            let cached_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["cursor".to_string()],
                None,
            );

            assert_eq!(cached_messages.len(), 1);
            assert_eq!(cached_messages[0].cost, 0.0);
        }
    }

    #[test]
    fn test_apply_pricing_if_available_keeps_existing_cost_without_pricing() {
        let mut msg = UnifiedMessage::new_with_agent(
            "roocode",
            "gpt-4o",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.42,
            Some("planner".to_string()),
        );

        apply_pricing_if_available(&mut msg, None);

        assert_eq!(msg.cost, 0.42);
    }

    #[test]
    fn test_apply_pricing_if_available_overrides_cost_when_pricing_exists() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-4o".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "codex",
            "gpt-4o",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.02);
        assert_eq!(msg.cost_source, CostSource::Estimated);
    }

    #[test]
    fn test_apply_pricing_if_available_marks_partial_subtotal() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "partial-model".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let mut msg = UnifiedMessage::new(
            "codex",
            "partial-model",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_write: 7,
                ..Default::default()
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert!((msg.cost - 0.01).abs() < 1e-12);
        assert_eq!(msg.cost_source, CostSource::PartiallyEstimated);
        assert_eq!(coverage_for_messages([&msg]), CostCoverage::Partial);
    }

    #[test]
    fn test_apply_pricing_if_available_marks_successful_zero_cost_lookup_estimated() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "free-model".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.0),
                output_cost_per_token: Some(0.0),
                cache_read_input_token_cost: Some(0.0),
                cache_creation_input_token_cost: Some(0.0),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let mut msg = UnifiedMessage::new(
            "codex",
            "free-model",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 3,
                cache_write: 7,
                reasoning: 2,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.0);
        assert_eq!(msg.cost_source, CostSource::Estimated);
        assert_eq!(coverage_for_messages([&msg]), CostCoverage::Complete);
    }

    #[test]
    fn test_apply_pricing_if_available_keeps_unknown_on_lookup_miss() {
        let pricing = pricing::PricingService::new(
            HashMap::from([(
                "other-model".into(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(0.001),
                    ..Default::default()
                },
            )]),
            HashMap::new(),
        );
        let mut msg = UnifiedMessage::new(
            "codex",
            "missing-model",
            "provider",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                ..Default::default()
            },
            0.25,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.25);
        assert_eq!(msg.cost_source, CostSource::Unknown);
    }

    #[test]
    fn test_apply_pricing_if_available_preserves_provider_reported_cost() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-4o".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let mut msg = UnifiedMessage::new(
            "opencode",
            "gpt-4o",
            "openai",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.42,
        );
        msg.mark_provider_reported_cost();

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.42);
        assert_eq!(msg.cost_source, CostSource::ProviderReported);
    }

    #[test]
    #[serial_test::serial]
    fn test_cost_provenance_matches_materialized_and_streaming_lanes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        let opencode_data_dir = source_home.path().join(".local/share/opencode");
        std::fs::create_dir_all(&opencode_data_dir).unwrap();
        let opencode_db =
            rusqlite::Connection::open(opencode_data_dir.join("opencode.db")).unwrap();
        opencode_db.execute_batch("CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);").unwrap();
        let sqlite_rows = [
            (
                "sqlite-a",
                r#"{"id":"json-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            ),
            (
                "sqlite-b",
                r#"{"id":"sqlite-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.06,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011201000}}"#,
            ),
            (
                "sqlite-c",
                r#"{"id":"both-authoritative","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.07,"tokens":{"input":20,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011202000}}"#,
            ),
            (
                "sqlite-d",
                r#"{"id":"both-estimated","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":40,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011203000}}"#,
            ),
        ];
        for (row_id, data) in sqlite_rows {
            opencode_db
                .execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![row_id, "oc-session", data],
                )
                .unwrap();
        }
        drop(opencode_db);

        let opencode_dir = opencode_data_dir.join("storage/message/project-1");
        std::fs::create_dir_all(&opencode_dir).unwrap();
        let json_rows = [
            (
                "a-json-authoritative.json",
                r#"{"id":"json-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.05,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
            ),
            (
                "b-json-estimated.json",
                r#"{"id":"sqlite-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011201000}}"#,
            ),
            (
                "c-json-authoritative.json",
                r#"{"id":"both-authoritative","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.08,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011202000}}"#,
            ),
            (
                "d-json-estimated.json",
                r#"{"id":"both-estimated","sessionID":"oc-session","role":"assistant","modelID":"gpt-4o","providerID":"openai","cost":0.0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011203000}}"#,
            ),
        ];
        for (name, data) in json_rows {
            std::fs::write(opencode_dir.join(name), data).unwrap();
        }

        let mut litellm = HashMap::new();
        litellm.insert(
            "openai/gpt-4o".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let clients = vec!["opencode".to_string()];
        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &clients,
            Some(&pricing),
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            Some(&pricing),
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );

        let summarize = |messages: Vec<UnifiedMessage>| {
            let mut rows: Vec<(String, f64, CostSource)> = messages
                .into_iter()
                .map(|message| {
                    (
                        message.dedup_key.unwrap_or_default(),
                        message.cost,
                        message.cost_source,
                    )
                })
                .collect();
            rows.sort_by(|left, right| left.0.cmp(&right.0));
            rows
        };
        let materialized = summarize(materialized);
        let streamed = summarize(streamed);
        assert_eq!(materialized, streamed);
        assert_eq!(
            materialized,
            vec![
                (
                    "both-authoritative".to_string(),
                    0.07,
                    CostSource::ProviderReported
                ),
                ("both-estimated".to_string(), 0.5, CostSource::Estimated),
                (
                    "json-authoritative".to_string(),
                    0.05,
                    CostSource::ProviderReported
                ),
                (
                    "sqlite-authoritative".to_string(),
                    0.06,
                    CostSource::ProviderReported
                ),
            ]
        );
    }

    #[test]
    fn test_apply_pricing_if_available_applies_zed_hosted_markup() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "zed",
            "claude-sonnet-4-5",
            crate::sessions::zed::ZED_HOSTED_PROVIDER,
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert!((msg.cost - 0.022).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_skips_zed_markup_for_non_zed_client() {
        // Non-zed client with provider_id "zed.dev" must not receive the +10%
        // markup. The multiplier is gated on (client == "zed" AND provider).
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claudecode",
            "claude-sonnet-4-5",
            crate::sessions::zed::ZED_HOSTED_PROVIDER,
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        // 10 * 0.001 + 5 * 0.002 = 0.020, no markup.
        assert!((msg.cost - 0.020).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_skips_zed_markup_for_byok_provider() {
        // A Zed message whose provider_id is the upstream provider directly
        // (BYOK / non-hosted path) must not be marked up — the user is paying
        // the upstream API directly, not through Zed.
        let mut litellm = HashMap::new();
        litellm.insert(
            "claude-sonnet-4-5".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "zed",
            "claude-sonnet-4-5",
            "anthropic",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert!((msg.cost - 0.020).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_reasoning_for_gemini() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "gemini",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 7,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.034);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_cache_read_pricing_for_gemini() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_read_input_token_cost: Some(0.0001),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "gemini",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 7,
                cache_write: 0,
                reasoning: 3,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.0267);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_market_rate_for_free_variant() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "z-ai/glm-4.7".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(HashMap::new(), openrouter);

        let mut msg = UnifiedMessage::new(
            "opencode",
            "glm-4.7-free",
            "modal",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.02);
    }

    #[test]
    fn test_apply_pricing_if_available_prefers_provider_aware_match() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "xai/grok-code-fast-1-0825".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "azure_ai/grok-code-fast-1".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "opencode",
            "grok-code",
            "azure",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_uses_nested_reseller_exact_match() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gpt-4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "azure/openai/gpt-4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gpt-4",
            "azure",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_keeps_scoped_fireworks_cost_without_exact_pricing() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "fireworks_ai/accounts/fireworks/models/deepseek-r1-0528-distill-qwen3-8b".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.0000002),
                output_cost_per_token: Some(0.0000002),
                ..Default::default()
            },
        );

        let mut openrouter = HashMap::new();
        openrouter.insert(
            "deepseek/deepseek-v4-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.000001),
                output_cost_per_token: Some(0.000002),
                ..Default::default()
            },
        );

        let pricing = pricing::PricingService::new(litellm, openrouter);
        let mut msg = UnifiedMessage::new(
            "opencode",
            "accounts/fireworks/models/deepseek-v4-pro",
            "fireworks",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.123,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.123);
    }

    #[test]
    fn test_apply_pricing_if_available_prefers_provider_specific_exact_match_over_plain_exact() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_creation_input_token_cost: None,
                ..Default::default()
            },
        );

        let mut openrouter = HashMap::new();
        openrouter.insert(
            "google/gemini-2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_creation_input_token_cost: Some(0.01),
                ..Default::default()
            },
        );

        let pricing = pricing::PricingService::new(litellm, openrouter);

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gemini-2.5-pro",
            "google",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 3,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.05);
    }

    #[test]
    fn test_apply_pricing_if_available_normalizes_openai_codex_provider() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "openai/gpt-5.2-preview".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        litellm.insert(
            "google/gpt-5.2-preview-max".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.1),
                output_cost_per_token: Some(0.2),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "openclaw",
            "gpt-5.2",
            "openai-codex",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_claude_code_gpt_5_3_codex() {
        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claude",
            "gpt-5.3-codex",
            "openai",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 1_000_000,
                output: 100_000,
                cache_read: 50_000,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        let expected = 1.75 + 1.4 + 0.00875;
        assert!((msg.cost - expected).abs() < 1e-12);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_claude_code_minimax_model() {
        let mut litellm = HashMap::new();
        litellm.insert(
            "minimax/minimax-m2.1".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());

        let mut msg = UnifiedMessage::new(
            "claude",
            "MiniMax-M2.1",
            "minimax",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        assert_eq!(msg.cost, 0.2);
    }

    #[test]
    fn test_apply_pricing_if_available_prices_kimi_k2p6_alias() {
        let mut openrouter = HashMap::new();
        openrouter.insert(
            "moonshotai/kimi-k2.6".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(9.5e-7),
                output_cost_per_token: Some(0.000004),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(HashMap::new(), openrouter);

        let mut msg = UnifiedMessage::new(
            "kimi",
            "k2p6",
            "kimi-for-coding",
            "session-1",
            1_776_000_000_000,
            TokenBreakdown {
                input: 1_000_000,
                output: 250_000,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(&pricing));

        let expected = 1_000_000.0 * 9.5e-7 + 250_000.0 * 0.000004;
        assert!((msg.cost - expected).abs() < 1e-12);
        assert!(msg.cost > 0.0);
    }

    #[test]
    fn test_select_local_parse_pricing_prefers_fresh_service_for_new_models() {
        let mut fresh_litellm = HashMap::new();
        fresh_litellm.insert(
            "gpt-5.4".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.000002),
                output_cost_per_token: Some(0.00001),
                ..Default::default()
            },
        );
        let fresh = Arc::new(pricing::PricingService::new(fresh_litellm, HashMap::new()));
        let stale = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let selected = select_local_parse_pricing(Ok(Arc::clone(&fresh)), || Some(stale)).unwrap();

        let mut msg = UnifiedMessage::new(
            "opencode",
            "gpt-5.4",
            "openai",
            "session-1",
            1_733_011_200_000,
            TokenBreakdown {
                input: 10,
                output: 5,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            0.0,
        );

        apply_pricing_if_available(&mut msg, Some(selected.as_ref()));

        assert!(msg.cost > 0.0);
    }

    #[test]
    fn test_select_local_parse_pricing_falls_back_to_stale_cache_on_fetch_error() {
        let mut stale_litellm = HashMap::new();
        stale_litellm.insert(
            "gpt-5.2".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.00000175),
                output_cost_per_token: Some(0.000014),
                ..Default::default()
            },
        );
        let stale = pricing::PricingService::new(stale_litellm, HashMap::new());

        let selected =
            select_local_parse_pricing(Err("network failed".to_string()), || Some(stale)).unwrap();

        assert!(selected.lookup_with_source("gpt-5.2", None).is_some());
    }

    #[test]
    fn test_select_local_parse_pricing_does_not_evaluate_stale_fallback_on_fresh_success() {
        let fresh = Arc::new(pricing::PricingService::new(HashMap::new(), HashMap::new()));
        let mut stale_called = false;

        let selected = select_local_parse_pricing(Ok(Arc::clone(&fresh)), || {
            stale_called = true;
            None
        })
        .unwrap();

        assert!(Arc::ptr_eq(&selected, &fresh));
        assert!(!stale_called);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn context_pricing_reads_captured_config_after_live_override_changes() {
        let root = tempfile::TempDir::new().unwrap();
        let config_a = root.path().join("config-a");
        let config_b = root.path().join("config-b");
        let cache_a = config_a.join("cache");
        let cache_b = config_b.join("cache");
        std::fs::create_dir_all(&cache_a).unwrap();
        std::fs::create_dir_all(&cache_b).unwrap();

        let write_cache = |path: &Path, data: &str| {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            std::fs::write(
                path,
                format!(r#"{{"timestamp":{timestamp},"data":{data}}}"#),
            )
            .unwrap();
        };
        let litellm_a = r#"{"litellm-context-drift":{"input_cost_per_token":0.000001,"output_cost_per_token":0.000002}}"#;
        let litellm_b = r#"{"litellm-context-drift":{"input_cost_per_token":0.000011,"output_cost_per_token":0.000012}}"#;
        let openrouter_a = r#"{"openrouter-context-drift":{"input_cost_per_token":0.000003,"output_cost_per_token":0.000004}}"#;
        let openrouter_b = r#"{"openrouter-context-drift":{"input_cost_per_token":0.000013,"output_cost_per_token":0.000014}}"#;
        write_cache(&cache_a.join("pricing-litellm.json"), litellm_a);
        write_cache(&cache_b.join("pricing-litellm.json"), litellm_b);
        write_cache(&cache_a.join("pricing-openrouter.json"), openrouter_a);
        write_cache(&cache_b.join("pricing-openrouter.json"), openrouter_b);

        std::fs::write(
            config_a.join("custom-pricing.json"),
            r#"{"models":{"custom-context-drift":{"input_cost_per_token":0.000005,"output_cost_per_token":0.000006}}}"#,
        )
        .unwrap();
        std::fs::write(
            config_b.join("custom-pricing.json"),
            r#"{"models":{"custom-context-drift":{"input_cost_per_token":0.000015,"output_cost_per_token":0.000016}}}"#,
        )
        .unwrap();

        let _env = EnvGuard::set(&[
            ("TOKSCALE_CONFIG_DIR", config_a.as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().join("home")),
            true,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        assert_eq!(context.pricing_config_dir(), config_a.as_path());

        unsafe { std::env::set_var("TOKSCALE_CONFIG_DIR", &config_b) };
        let service = load_pricing_for_local_parse_with_context(Some(&context))
            .await
            .expect("captured pricing cache should load");

        assert_eq!(
            service
                .lookup_with_source("litellm-context-drift", Some("litellm"))
                .unwrap()
                .pricing
                .input_cost_per_token,
            Some(0.000001)
        );
        assert_eq!(
            service
                .lookup_with_source("openrouter-context-drift", Some("openrouter"))
                .unwrap()
                .pricing
                .input_cost_per_token,
            Some(0.000003)
        );
        assert_eq!(
            service
                .lookup_with_source("custom-context-drift", Some("custom"))
                .unwrap()
                .pricing
                .input_cost_per_token,
            Some(0.000005)
        );
    }

    #[test]
    fn test_dedupe_latest_trae_messages_keeps_latest_timestamp_for_session() {
        let messages = vec![
            make_trae_message(
                "session-stable",
                1_700_000_002_000,
                Some("trae:session-stable:1_700_000_002"),
                0.2,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_003_000,
                Some("trae:session-stable:1_700_000_003"),
                0.3,
            ),
            make_trae_message(
                "session-other",
                1_700_000_001_000,
                Some("trae:session-other:1_700_000_001"),
                0.1,
            ),
        ];

        let deduped = dedupe_latest_trae_messages(messages);

        assert_eq!(deduped.len(), 2);
        let stable = deduped
            .iter()
            .find(|msg| msg.session_id == "session-stable")
            .expect("session-stable should remain after dedupe");
        assert_eq!(stable.timestamp, 1_700_000_003_000);
        assert_eq!(stable.cost, 0.3);
        assert_eq!(
            stable.dedup_key.as_deref(),
            Some("trae:session-stable:1_700_000_003")
        );
    }

    #[test]
    fn test_dedupe_latest_trae_messages_tiebreaks_by_dedup_key() {
        let messages = vec![
            make_trae_message(
                "session-stable",
                1_700_000_010_000,
                Some("dedupe-key-a"),
                0.2,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_010_000,
                Some("dedupe-key-z"),
                0.4,
            ),
            make_trae_message(
                "session-stable",
                1_700_000_009_000,
                Some("dedupe-key-m"),
                0.1,
            ),
        ];

        let deduped = dedupe_latest_trae_messages(messages);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].timestamp, 1_700_000_010_000);
        assert_eq!(deduped[0].dedup_key.as_deref(), Some("dedupe-key-z"));
        assert_eq!(deduped[0].cost, 0.4);
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_with_pricing_keeps_gateway_message_under_synthetic_filter() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), temp_dir.path());
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"hf:deepseek-ai/DeepSeek-V3-0324","providerID":"unknown","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["synthetic".to_string()],
            Some(&pricing),
        );

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "opencode");
        assert_eq!(messages[0].model_id, "deepseek-v3-0324");
        assert_eq!(messages[0].provider_id, "synthetic");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_preserves_gateway_message_client_counts() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string(), "synthetic".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::OpenCode), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "opencode");
        assert_eq!(parsed.messages[0].model_id, "deepseek-v3-0324");
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(parsed.messages[0].provider_id, "fireworks_ai");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_fireworks_provider_kept_under_synthetic_only_filter() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), temp_dir.path());
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0.1,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let pricing = pricing::PricingService::new(HashMap::new(), HashMap::new());
        let messages = parse_all_messages_with_pricing(
            temp_dir.path().to_str().unwrap(),
            &["synthetic".to_string()],
            Some(&pricing),
        );

        assert_eq!(
            messages.len(),
            1,
            "fireworks gateway message must not be dropped when filtering for synthetic"
        );
        assert_eq!(messages[0].client, "opencode");
        assert_eq!(messages[0].model_id, "deepseek-v3-0324");
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(messages[0].provider_id, "fireworks_ai");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_fireworks_provider_kept_under_synthetic_only_filter() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let message_dir = temp_dir
            .path()
            .join(".local/share/opencode/storage/message/project-1");
        std::fs::create_dir_all(&message_dir).unwrap();
        std::fs::write(
            message_dir.join("msg_001.json"),
            r#"{"id":"msg-1","sessionID":"session-1","role":"assistant","modelID":"accounts/fireworks/models/deepseek-v3-0324","providerID":"fireworks","cost":0.1,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1733011200000}}"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["synthetic".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            parsed.messages.len(),
            1,
            "fireworks gateway message must not be dropped when filtering for synthetic only"
        );
        assert_eq!(parsed.messages[0].client, "opencode");
        assert_eq!(parsed.messages[0].model_id, "deepseek-v3-0324");
        // opencode canonicalizes the raw "fireworks" gateway id to "fireworks_ai" (#760).
        assert_eq!(parsed.messages[0].provider_id, "fireworks_ai");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_honors_scanner_settings_opencode_db_paths() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        // Regression guard: `parse_local_clients` used to call
        // `scan_all_clients_with_env_strategy`, which silently dropped
        // `options.scanner_settings`. Users with
        // `scanner.opencodeDbPaths` pointing at an OPENCODE_DB outside the
        // XDG data dir would see no rows through the clients/wrapped
        // command paths even though model/monthly/graph reports honored
        // the same config.
        let temp_dir = tempfile::TempDir::new().unwrap();
        // Deliberately do not create ~/.local/share/opencode so nothing
        // is auto-discoverable; the only db the scanner can find must
        // come from `scanner_settings`.
        let outside_dir = temp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let external_db = outside_dir.join("opencode.db");

        let conn = rusqlite::Connection::open(&external_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "ext-msg-1",
                "ext-session",
                r#"{
                    "role": "assistant",
                    "modelID": "claude-sonnet-4",
                    "providerID": "anthropic",
                    "tokens": { "input": 42, "output": 7, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    "time": { "created": 1700000000000.0 }
                }"#
            ],
        )
        .unwrap();
        drop(conn);

        // Without scanner_settings: no rows (nothing auto-discoverable).
        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::OpenCode), 0);
        assert!(parsed_default.messages.is_empty());

        // With scanner_settings pointing at the external db: the user
        // row must show up.
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["opencode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                opencode_db_paths: vec![external_db.clone()],
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();
        assert_eq!(
            parsed_with_settings.counts.get(ClientId::OpenCode),
            1,
            "scanner.opencodeDbPaths must reach the parse_local_clients path"
        );
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "opencode");
        assert_eq!(parsed_with_settings.messages[0].model_id, "claude-sonnet-4");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_honors_scanner_extra_scan_paths_for_hermes_profile_db() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let profile_dir = temp_dir.path().join("external-hermes/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-extra-session",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(conn);

        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::Hermes), 0);
        assert!(parsed_default.messages.is_empty());

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("hermes".to_string(), vec![profile_dir]);
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed_with_settings.counts.get(ClientId::Hermes), 2);
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "hermes");
        assert_eq!(
            parsed_with_settings.messages[0].agent.as_deref(),
            Some("Hermes Agent")
        );
        assert_eq!(
            parsed_with_settings.messages[0].session_id,
            "hermes-extra-session"
        );
        assert_eq!(parsed_with_settings.messages[0].model_id, "claude-sonnet-4");
        assert_eq!(parsed_with_settings.messages[0].input, 100);
        assert_eq!(parsed_with_settings.messages[0].output, 25);
    }

    #[test]
    #[serial_test::serial]
    fn test_auto_discovered_hermes_profile_reaches_all_consumers() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let count_cache = tempfile::TempDir::new().unwrap();
        let profile_dir = source_home.path().join(".hermes/profiles/research");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-auto-profile",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(conn);

        let home = source_home.path().to_str().unwrap().to_string();
        let clients = vec!["hermes".to_string()];
        let materialized = with_isolated_tokscale_cache(materialized_cache.path(), || {
            parse_all_messages_with_pricing_with_env_strategy(
                &home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        });
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].session_id, "hermes-auto-profile");
        assert_eq!(materialized[0].tokens.input, 100);
        assert_eq!(materialized[0].tokens.output, 25);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let streaming = with_isolated_tokscale_cache(streaming_cache.path(), || {
            runtime
                .block_on(get_model_report(ReportOptions {
                    home_dir: Some(home.clone()),
                    use_env_roots: false,
                    clients: Some(clients.clone()),
                    ..Default::default()
                }))
                .unwrap()
        });
        assert_eq!(streaming.total_messages, 2);
        assert_eq!(streaming.total_input, 100);
        assert_eq!(streaming.total_output, 25);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let counted = with_isolated_tokscale_cache(count_cache.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(future_ms),
            })
            .unwrap()
        });
        assert_eq!(counted.counts.get(ClientId::Hermes), 2);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(counted.messages[0].session_id, "hermes-auto-profile");
        assert_eq!(counted.messages[0].input, 100);
        assert_eq!(counted.messages[0].output, 25);
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_never_prunes_hermes_dbs_from_extra_scan_paths() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        // SQLite WAL writes may leave the main db file's mtime untouched, so
        // `modified_after` must not prune Hermes/Zed dbs even when they come
        // from user scan roots (the `files` lanes) rather than the default
        // single-db path. A threshold in the future would prune any mtime.
        let temp_dir = tempfile::TempDir::new().unwrap();
        let profile_dir = temp_dir.path().join("external-hermes/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &conn,
            "hermes-wal-session",
            "claude-sonnet-4",
            1,
            50,
            10,
            0.03,
        );
        drop(conn);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("hermes".to_string(), vec![profile_dir]);
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: Some(future_ms),
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Hermes), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].session_id, "hermes-wal-session");
    }

    const AGY_PRUNE_THRESHOLD_MS: u64 = 1_700_000_000_000;

    /// Set a file's mtime to `AGY_PRUNE_THRESHOLD_MS + offset_ms`.
    fn set_mtime_relative_to_agy_threshold(path: &std::path::Path, offset_ms: i64) {
        let ms = (AGY_PRUNE_THRESHOLD_MS as i64 + offset_ms) as u64;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
            .unwrap();
    }

    /// Prune a scan holding one Antigravity CLI db (plus an old Claude log as a
    /// control) and report whether the db survived. `db_offset_ms` is the db
    /// mtime relative to the threshold, `wal_offset_ms` the optional `-wal`
    /// sidecar's. The Claude control must always be pruned.
    fn antigravity_cli_db_survives_prune(db_offset_ms: i64, wal_offset_ms: Option<i64>) -> bool {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cli_db = temp_dir.path().join("conv.db");
        std::fs::File::create(&cli_db).unwrap();
        set_mtime_relative_to_agy_threshold(&cli_db, db_offset_ms);
        if let Some(offset) = wal_offset_ms {
            let wal = temp_dir.path().join("conv.db-wal");
            std::fs::File::create(&wal).unwrap();
            set_mtime_relative_to_agy_threshold(&wal, offset);
        }
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();
        set_mtime_relative_to_agy_threshold(&claude_log, -1_000);

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::AntigravityCli)
            .push(cli_db.clone());
        scan_result.get_mut(ClientId::Claude).push(claude_log);
        crate::prune_scan_result_by_mtime(&mut scan_result, AGY_PRUNE_THRESHOLD_MS);

        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "control: a plain-file client's stale log is pruned"
        );
        !scan_result.get(ClientId::AntigravityCli).is_empty()
    }

    #[cfg(not(windows))]
    #[test]
    fn test_modified_after_prunes_old_antigravity_cli_db_without_wal() {
        assert!(!antigravity_cli_db_survives_prune(-1_000, None));
    }

    #[cfg(not(windows))]
    #[test]
    fn test_modified_after_prunes_antigravity_cli_db_when_db_and_wal_are_old() {
        assert!(!antigravity_cli_db_survives_prune(-2_000, Some(-1_000)));
    }

    /// NTFS mtime behaviour is unmeasured, so Windows keeps every agy db.
    #[cfg(windows)]
    #[test]
    fn test_modified_after_keeps_antigravity_cli_dbs_on_windows() {
        assert!(antigravity_cli_db_survives_prune(-2_000, Some(-1_000)));
    }

    #[test]
    fn test_modified_after_keeps_antigravity_cli_db_when_only_wal_is_fresh() {
        assert!(antigravity_cli_db_survives_prune(-1_000, Some(1_000)));
    }

    #[test]
    fn test_modified_after_keeps_fresh_antigravity_cli_db_without_wal() {
        assert!(antigravity_cli_db_survives_prune(1_000, None));
        // An mtime exactly at the threshold is kept.
        assert!(antigravity_cli_db_survives_prune(0, None));
    }

    /// A checkpoint moves pages from `-wal` into the db, so the db can be the
    /// fresh one while `-wal` still carries an older mtime.
    #[test]
    fn test_modified_after_keeps_antigravity_cli_db_when_only_db_is_fresh() {
        assert!(antigravity_cli_db_survives_prune(1_000, Some(-1_000)));
    }

    /// A symlinked db's WAL sits beside the link or the target depending on the
    /// writer's SQLite build, so the db is never pruned. Control: the target
    /// itself, with the same old db and `-wal`, is.
    #[cfg(unix)]
    #[test]
    fn test_modified_after_keeps_symlinked_antigravity_cli_db() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let target = temp_dir.path().join("elsewhere.db");
        std::fs::File::create(&target).unwrap();
        set_mtime_relative_to_agy_threshold(&target, -2_000);
        let target_wal = temp_dir.path().join("elsewhere.db-wal");
        std::fs::File::create(&target_wal).unwrap();
        set_mtime_relative_to_agy_threshold(&target_wal, -1_000);
        let link = temp_dir.path().join("conv.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::AntigravityCli)
            .extend([link.clone(), target.clone()]);
        crate::prune_scan_result_by_mtime(&mut scan_result, AGY_PRUNE_THRESHOLD_MS);
        assert_eq!(
            scan_result.get(ClientId::AntigravityCli),
            std::slice::from_ref(&link)
        );
    }

    #[test]
    fn test_modified_after_keeps_antigravity_cli_db_when_stat_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let missing_db = temp_dir.path().join("gone.db");
        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::AntigravityCli)
            .push(missing_db.clone());
        crate::prune_scan_result_by_mtime(&mut scan_result, AGY_PRUNE_THRESHOLD_MS);
        assert_eq!(
            scan_result.get(ClientId::AntigravityCli),
            std::slice::from_ref(&missing_db),
            "a db whose freshness cannot be established must fail open"
        );
    }

    /// A `-wal` stat that fails for a reason other than absence keeps the db.
    /// Here the `-wal` name is one byte past NAME_MAX (255), so it fails with
    /// ENAMETOOLONG while the old db itself stats fine.
    #[cfg(not(windows))]
    #[test]
    fn test_modified_after_keeps_antigravity_cli_db_when_wal_stat_fails() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = temp_dir.path().join(format!("{}.db", "a".repeat(252 - 3)));
        std::fs::File::create(&db).unwrap();
        set_mtime_relative_to_agy_threshold(&db, -1_000);
        let wal_error =
            std::fs::metadata(message_cache::append_path_suffix(&db, "-wal")).unwrap_err();
        assert_ne!(
            wal_error.kind(),
            std::io::ErrorKind::NotFound,
            "fixture: not a missing -wal"
        );

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::AntigravityCli)
            .push(db.clone());
        crate::prune_scan_result_by_mtime(&mut scan_result, AGY_PRUNE_THRESHOLD_MS);
        assert_eq!(
            scan_result.get(ClientId::AntigravityCli),
            std::slice::from_ref(&db)
        );
    }

    /// agy writes in WAL mode: a turn lands in `-wal` frames and the db's mtime
    /// stays put until a checkpoint. The writer stays open with automatic
    /// checkpoints off, so the only copy of the message is in the WAL.
    #[cfg(not(windows))]
    #[test]
    #[serial_test::serial]
    fn test_modified_after_parses_antigravity_cli_db_only_when_db_or_wal_is_fresh() {
        // Hermetic durable store: `use_env_roots: false` keeps scan roots under
        // the explicit home, but the usage dir still binds from ambient env, so
        // without this a concurrent env-mutating test can relocate or contend
        // the store mid-ingest (mirrors the adjacent streaming AGY test).
        // `SYRTIS_USAGE_DIR` has top precedence in `resolve_usage_dir`, so it
        // stays put under parallel non-serial `HOME`/`TOKSCALE_CONFIG_DIR`
        // mutators; `#[serial]` excludes the remaining serial mutators.
        let cache_home = tempfile::TempDir::new().unwrap();
        let usage_dir = cache_home.path().join("usage");
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("SYRTIS_USAGE_DIR", usage_dir.as_os_str()),
        ]);
        let temp_dir = tempfile::TempDir::new().unwrap();
        let conversations_dir = temp_dir
            .path()
            .join(".gemini/antigravity-cli/conversations");
        let _writer =
            write_antigravity_cli_db_with(&conversations_dir, "conv-e2e", "resp-e2e", true);
        let db = conversations_dir.join("conv-e2e.db");
        let wal = conversations_dir.join("conv-e2e.db-wal");
        assert!(
            wal.metadata().unwrap().len() > 0,
            "the row is held in WAL frames"
        );
        set_mtime_relative_to_agy_threshold(&db, -2_000);

        let parse = || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["antigravity-cli".to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(AGY_PRUNE_THRESHOLD_MS),
            })
            .unwrap()
        };

        set_mtime_relative_to_agy_threshold(&wal, 1_000);
        assert_eq!(parse().messages.len(), 1, "fresh -wal keeps the db parsed");

        set_mtime_relative_to_agy_threshold(&wal, -1_000);
        assert!(
            parse().messages.is_empty(),
            "db and -wal both older than the threshold are not parsed"
        );
    }

    /// Write a minimal Antigravity CLI conversation DB (one priced
    /// `gen_metadata` row carrying `response_id`). The `trajectory_metadata_blob`
    /// table is omitted on purpose — the parser tolerates its absence and falls
    /// back to the file mtime for the timestamp.
    fn write_antigravity_cli_db(
        conversations_dir: &std::path::Path,
        file_stem: &str,
        response_id: &str,
    ) {
        write_antigravity_cli_db_with(conversations_dir, file_stem, response_id, false);
    }

    /// As `write_antigravity_cli_db`, returning the open writer. With `wal`, the
    /// db is in WAL mode with automatic checkpoints off, so while the returned
    /// connection lives the row exists only in `-wal`.
    fn write_antigravity_cli_db_with(
        conversations_dir: &std::path::Path,
        file_stem: &str,
        response_id: &str,
        wal: bool,
    ) -> rusqlite::Connection {
        fn encode_varint(mut value: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let mut byte = (value & 0x7f) as u8;
                value >>= 7;
                if value != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if value == 0 {
                    break;
                }
            }
            out
        }
        fn enc_varint(field: u64, value: u64) -> Vec<u8> {
            let mut out = encode_varint(field << 3);
            out.extend(encode_varint(value));
            out
        }
        fn enc_len(field: u64, payload: &[u8]) -> Vec<u8> {
            let mut out = encode_varint((field << 3) | 2);
            out.extend(encode_varint(payload.len() as u64));
            out.extend_from_slice(payload);
            out
        }

        let mut usage = Vec::new();
        usage.extend(enc_varint(2, 500)); // new input
        usage.extend(enc_varint(9, 300)); // output
        usage.extend(enc_len(11, response_id.as_bytes())); // responseId
        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let gen_blob = enc_len(1, &chat_model);

        std::fs::create_dir_all(conversations_dir).unwrap();
        let path = conversations_dir.join(format!("{file_stem}.db"));
        let conn = rusqlite::Connection::open(&path).unwrap();
        if wal {
            let mode: String = conn
                .query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_lowercase(), "wal");
            conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
        }
        conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
            .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            rusqlite::params![gen_blob],
        )
        .unwrap();
        conn
    }

    // Two independent Antigravity CLI conversation DBs that reuse the same
    // responseId must both survive the streaming report path. responseIds are
    // unique only within a conversation, so the cross-file dedup gate is
    // namespaced by session; with a bare-responseId key (the pre-fix behaviour)
    // the second conversation is silently dropped and this fails (count == 1).
    #[test]
    #[serial_test::serial]
    fn test_streaming_antigravity_cli_keeps_colliding_response_ids_across_conversations() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let conversations_dir = source_home
                .path()
                .join(".gemini/antigravity-cli/conversations");
            write_antigravity_cli_db(&conversations_dir, "conv-aaa", "SHARED");
            write_antigravity_cli_db(&conversations_dir, "conv-bbb", "SHARED");

            let mut sessions: Vec<String> = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["antigravity-cli".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| sessions.push(m.session_id.clone()),
            );

            sessions.sort();
            assert_eq!(
                sessions,
                vec!["conv-aaa".to_string(), "conv-bbb".to_string()],
                "both conversations reusing responseId \"SHARED\" must survive"
            );
        }
    }

    // The `steps` turn time must date the row identically on the materialized,
    // streaming and count lanes; each lane calls the parser on its own.
    #[test]
    #[serial_test::serial]
    fn test_antigravity_cli_three_lanes_parity_with_step_timestamp() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        let conv_dir = source_home
            .path()
            .join(".gemini/antigravity-cli/conversations");
        write_antigravity_cli_db(&conv_dir, "conv-parity", "resp-parity-1");

        // steps.metadata = {#1: {#1: seconds}, #9: {#11: responseId}}
        let step_seconds: u64 = 1_789_200_000;
        let mut ts = vec![0x08];
        let mut v = step_seconds;
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            ts.push(if v == 0 { byte } else { byte | 0x80 });
            if v == 0 {
                break;
            }
        }
        let resp = b"resp-parity-1";
        let mut m9 = vec![(11 << 3) | 2, resp.len() as u8];
        m9.extend_from_slice(resp);
        let mut metadata = vec![(1 << 3) | 2, ts.len() as u8];
        metadata.extend(ts);
        metadata.extend([(9 << 3) | 2, m9.len() as u8]);
        metadata.extend(m9);
        let conn = rusqlite::Connection::open(conv_dir.join("conv-parity.db")).unwrap();
        conn.execute_batch("CREATE TABLE steps (idx integer, step_type integer, metadata blob);")
            .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            rusqlite::params![metadata],
        )
        .unwrap();
        drop(conn);
        let expected_ts = step_seconds as i64 * 1000;

        let mat_messages = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["antigravity-cli".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(mat_messages.len(), 1);
        assert_eq!(mat_messages[0].timestamp, expected_ts);

        let mut stream_messages = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["antigravity-cli".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_m: &UnifiedMessage| true,
            &mut |m: &UnifiedMessage| stream_messages.push(m.clone()),
        );
        assert_eq!(stream_messages.len(), 1);
        assert_eq!(stream_messages[0].timestamp, expected_ts);

        let count_result = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().to_string()),
            clients: Some(vec!["antigravity-cli".to_string()]),
            use_env_roots: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(count_result.messages.len(), 1);
        assert_eq!(count_result.messages[0].timestamp, expected_ts);
    }

    // jcode (`~/.jcode/sessions/session_*.json`) must be discovered by the
    // generic scanner (EnvVar JCODE_HOME / .jcode root, `session_*.json` glob)
    // and flow through the streaming lane with its authoritative per-message
    // token_usage.
    #[test]
    #[serial_test::serial]
    fn test_streaming_jcode_flows_through_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let sessions_dir = source_home.path().join(".jcode/sessions");
            std::fs::create_dir_all(&sessions_dir).unwrap();
            std::fs::write(
                sessions_dir.join("session_test.json"),
                r#"{"id":"session_test","provider_key":"cliproxyapi","model":"claude-sonnet-4","working_dir":"/x","messages":[{"id":"u1","role":"user","timestamp":"2026-06-16T12:00:00Z"},{"id":"a1","role":"assistant","timestamp":"2026-06-16T12:00:01Z","token_usage":{"input_tokens":1200,"output_tokens":300}}]}"#,
            )
            .unwrap();

            let mut input_sum = 0i64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["jcode".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    input_sum += m.tokens.input;
                    count += 1;
                },
            );

            assert_eq!(
                count, 1,
                "the jcode assistant message must flow through the streaming lane"
            );
            assert_eq!(input_sum, 1200);
        }
    }

    #[test]
    #[serial_test::serial]
    fn m21_sources_keep_materialized_streaming_count_and_report_parity() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let report_cache = tempfile::TempDir::new().unwrap();
        let clients = vec![
            "kimi".to_string(),
            "junie".to_string(),
            "opencodereview".to_string(),
        ];

        let legacy_kimi = source_home
            .path()
            .join(".kimi/sessions/group/legacy-session/wire.jsonl");
        std::fs::create_dir_all(legacy_kimi.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_kimi,
            r#"{"timestamp":1770983410.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":7,"output":3},"message_id":"legacy-1"}}}"#,
        )
        .unwrap();

        let kimi_code = source_home
            .path()
            .join(".kimi-code/sessions/workspace/code-session/agents/main/wire.jsonl");
        std::fs::create_dir_all(kimi_code.parent().unwrap()).unwrap();
        let code_a = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":50,"inputCacheRead":10,"inputCacheCreation":0},"usageScope":"turn","time":1770983420000,"turnId":"turn-a"}"#;
        let code_b = r#"{"type":"usage.record","model":"kimi-code/kimi-for-coding","usage":{"inputOther":100,"output":50,"inputCacheRead":10,"inputCacheCreation":0},"usageScope":"turn","time":1770983420001,"turnId":"turn-b"}"#;
        std::fs::write(&kimi_code, format!("{code_a}\n{code_a}\n{code_b}\n")).unwrap();
        let kimi_code_replay = source_home
            .path()
            .join(".kimi-code/sessions/workspace/code-session/agents/reviewer/wire.jsonl");
        std::fs::create_dir_all(kimi_code_replay.parent().unwrap()).unwrap();
        std::fs::write(&kimi_code_replay, format!("{code_a}\n")).unwrap();

        let junie = source_home
            .path()
            .join(".junie/sessions/session-260213-120000/events.jsonl");
        std::fs::create_dir_all(junie.parent().unwrap()).unwrap();
        let write_junie = |input: i64, cost: f64| {
            let usage = serde_json::json!({
                "timestampMs": 1_770_983_430_000_i64,
                "event": {
                    "agentEvent": {
                        "kind": "LlmResponseMetadataEvent",
                        "agent": { "name": "reviewer" },
                        "modelUsage": [{
                            "model": "gpt-5",
                            "provider": "openai",
                            "inputTokens": input,
                            "outputTokens": 50,
                            "time": 2_000,
                            "cost": cost
                        }]
                    }
                }
            });
            std::fs::write(
                &junie,
                format!("{}\n{usage}\n", r#"{"kind":"UserPromptEvent"}"#),
            )
            .unwrap();
        };
        write_junie(100, 0.125);

        let review = source_home
            .path()
            .join(".opencodereview/sessions/repo/review-session.jsonl");
        std::fs::create_dir_all(review.parent().unwrap()).unwrap();
        let review_contents = concat!(
            r#"{"type":"session_start","cwd":"/work/repo"}"#,
            "\n",
            r#"{"type":"llm_response","timestamp":"2026-02-13T12:00:40Z","model":"gpt-4o","duration_ms":1500,"usage":{"prompt_tokens":20,"completion_tokens":5,"cache_read_tokens":1,"cache_write_tokens":2}}"#,
            "\n"
        );
        std::fs::write(&review, review_contents).unwrap();

        let home = source_home.path().to_string_lossy().into_owned();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                let mut messages = parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                    None,
                );
                messages.sort_by(|left, right| {
                    (&left.client, &left.session_id, &left.dedup_key).cmp(&(
                        &right.client,
                        &right.session_id,
                        &right.dedup_key,
                    ))
                });
                messages
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_| true,
                    &mut |message| messages.push(message.clone()),
                );
                messages.sort_by(|left, right| {
                    (&left.client, &left.session_id, &left.dedup_key).cmp(&(
                        &right.client,
                        &right.session_id,
                        &right.dedup_key,
                    ))
                });
                messages
            })
        };

        let cold_materialized = run_materialized();
        let cold_streaming = run_streaming();
        assert_eq!(cold_materialized, cold_streaming);
        assert_eq!(cold_materialized.len(), 5);
        assert_eq!(
            cold_materialized
                .iter()
                .filter(|message| message.client == "kimi")
                .count(),
            3
        );
        let junie_message = cold_materialized
            .iter()
            .find(|message| message.client == "junie")
            .unwrap();
        assert_eq!(junie_message.cost_source, CostSource::ProviderReported);
        assert!((junie_message.cost - 0.125).abs() < 1e-9);
        assert!(junie_message.is_turn_start);
        assert_eq!(junie_message.duration_ms, Some(2_000));
        let review_message = cold_materialized
            .iter()
            .find(|message| message.client == "opencodereview")
            .unwrap();
        assert_eq!(review_message.duration_ms, Some(1_500));
        assert_eq!(review_message.workspace_label.as_deref(), Some("repo"));
        assert_eq!(run_materialized(), cold_materialized);
        assert_eq!(run_streaming(), cold_streaming);

        write_junie(200, 0.25);
        let rewritten_materialized = run_materialized();
        let rewritten_streaming = run_streaming();
        assert_eq!(rewritten_materialized, rewritten_streaming);
        let rewritten_junie = rewritten_materialized
            .iter()
            .find(|message| message.client == "junie" && message.tokens.input == 200)
            .unwrap();
        assert_eq!(rewritten_junie.tokens.input, 200);
        assert!((rewritten_junie.cost - 0.25).abs() < 1e-9);

        std::fs::remove_file(&review).unwrap();
        assert_eq!(run_materialized(), rewritten_materialized);
        assert_eq!(run_streaming(), rewritten_streaming);
        std::fs::write(&review, review_contents).unwrap();

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.clone()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Kimi), 3);
        assert_eq!(counted.counts.get(ClientId::Junie), 1);
        assert_eq!(counted.counts.get(ClientId::OpenCodeReview), 1);
        assert_eq!(counted.messages.len(), 5);

        with_isolated_tokscale_cache(report_cache.path(), || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let options = ReportOptions {
                home_dir: Some(home),
                use_env_roots: false,
                clients: Some(clients),
                ..Default::default()
            };
            let model = runtime.block_on(get_model_report(options.clone())).unwrap();
            let monthly = runtime
                .block_on(get_monthly_report(options.clone()))
                .unwrap();
            let hourly = runtime
                .block_on(get_hourly_report(options.clone()))
                .unwrap();
            let agents = runtime.block_on(get_agents_report(options)).unwrap();
            assert_eq!(model.total_messages, 5);
            assert_eq!(model.total_input, 427);
            assert_eq!(model.total_output, 158);
            assert_eq!(model.total_cache_read, 21);
            assert_eq!(model.total_cache_write, 2);
            assert!((model.total_cost - 0.25).abs() < 1e-9);
            assert_eq!(
                monthly
                    .entries
                    .iter()
                    .map(|entry| entry.message_count)
                    .sum::<i32>(),
                5
            );
            assert_eq!(
                hourly
                    .entries
                    .iter()
                    .map(|entry| entry.message_count)
                    .sum::<i32>(),
                5
            );
            assert_eq!(agents.total_messages, 5);
            assert_eq!(
                agents.entries.iter().map(|entry| entry.input).sum::<i64>(),
                427
            );
        });
    }

    // micode (`$XDG_DATA_HOME/micode/*.db`, WAL-mode SQLite) must be discovered
    // via the generic `*.db` glob and flow through the streaming lane, keeping
    // its authoritative per-message cost intact (MiMo models are unpriced, so
    // apply_pricing leaves the embedded cost alone).
    #[test]
    #[serial_test::serial]
    fn test_streaming_micode_flows_with_authoritative_cost() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let micode_dir = source_home.path().join(".local/share/mimocode");
            std::fs::create_dir_all(&micode_dir).unwrap();
            let db_path = micode_dir.join("test.db");
            {
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                conn.execute_batch(
                    "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, data TEXT NOT NULL);",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![
                        "msg_001",
                        "ses_001",
                        r#"{"role":"assistant","modelID":"mimo-v2.5-pro","providerID":"mimo","cost":0.05,"tokens":{"input":1000,"output":500},"time":{"created":1700000000000.0,"completed":1700000001000.0}}"#
                    ],
                )
                .unwrap();
            }

            let mut cost_sum = 0.0f64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["micode".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    cost_sum += m.cost;
                    count += 1;
                },
            );

            assert_eq!(
                count, 1,
                "the micode assistant message must flow through the streaming lane"
            );
            assert!(
                (cost_sum - 0.05).abs() < 1e-9,
                "authoritative micode cost must survive pricing (got {cost_sum})"
            );
        }
    }

    // #742 Part 2: the micode lane is cost-guarded so MiMo Code's authoritative
    // embedded cost is never overwritten by a recomputed tokens*rate when the
    // model resolves to a price. reprice_lane_message(.., guard=true) reprices
    // only when the embedded cost is absent (<= 0.0); guard=false is the old
    // unconditional behavior that this fix replaces for micode.
    #[test]
    fn test_reprice_lane_message_guards_authoritative_micode_cost() {
        // A pricing service that WOULD recompute a large cost for the MiMo model
        // (1000*0.001 + 500*0.002 = 2.0, versus the embedded 0.05).
        let mut litellm = HashMap::new();
        litellm.insert(
            "mimo-v2.5-pro".into(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let recomputed = 1000.0 * 0.001 + 500.0 * 0.002; // 2.0

        let make = |embedded_cost: f64| {
            UnifiedMessage::new(
                "micode",
                "mimo-v2.5-pro",
                "mimo",
                "ses",
                1_700_000_000_000,
                TokenBreakdown {
                    input: 1000,
                    output: 500,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                embedded_cost,
            )
        };

        // guard=true + embedded cost present -> authoritative cost survives.
        let mut guarded = make(0.05);
        reprice_lane_message(&mut guarded, Some(&pricing), true);
        assert!(
            (guarded.cost - 0.05).abs() < 1e-9,
            "cost-guarded reprice must keep the embedded 0.05, got {}",
            guarded.cost
        );

        // guard=false (old behavior) -> unconditionally overwritten by the recompute.
        let mut unguarded = make(0.05);
        reprice_lane_message(&mut unguarded, Some(&pricing), false);
        assert!(
            (unguarded.cost - recomputed).abs() < 1e-9,
            "unguarded reprice overwrites the embedded cost with {recomputed}, got {}",
            unguarded.cost
        );

        // guard=true + embedded cost absent (<= 0.0) -> still repriced (fallback).
        let mut absent = make(0.0);
        reprice_lane_message(&mut absent, Some(&pricing), true);
        assert!(
            (absent.cost - recomputed).abs() < 1e-9,
            "a missing embedded cost must still be priced, got {}",
            absent.cost
        );
    }

    fn roo_task_root(home: &Path, client: ClientId) -> PathBuf {
        let relative = match client {
            ClientId::RooCode => ".config/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks",
            ClientId::KiloCode => ".config/Code/User/globalStorage/kilocode.kilo-code/tasks",
            ClientId::Cline => ".config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks",
            _ => panic!("not a Roo-family client: {client:?}"),
        };
        home.join(relative)
    }

    fn write_roo_history_fixture(history: &Path, model: &str, agent: &str) {
        std::fs::write(
            history,
            format!(
                "<environment_details><model>{model}</model><slug>{agent}</slug></environment_details>"
            ),
        )
        .unwrap();
    }

    fn write_roo_task_fixture(
        home: &Path,
        client: ClientId,
        task_id: &str,
        model: &str,
        agent: &str,
    ) -> (PathBuf, PathBuf) {
        let task_dir = roo_task_root(home, client).join(task_id);
        std::fs::create_dir_all(&task_dir).unwrap();
        let ui_messages = task_dir.join("ui_messages.json");
        std::fs::write(
            &ui_messages,
            r#"[{"type":"say","say":"api_req_started","ts":"2026-06-25T10:00:00Z","text":"{\"cost\":0.125,\"tokensIn\":100,\"tokensOut\":25,\"cacheReads\":10,\"cacheWrites\":5,\"apiProtocol\":\"anthropic\"}"}]"#,
        )
        .unwrap();
        let history = sessions::roocode::history_path_for_ui_messages(&ui_messages);
        write_roo_history_fixture(&history, model, agent);
        (ui_messages, history)
    }

    #[test]
    #[serial_test::serial]
    fn roo_sibling_annotations_do_not_erase_archived_ui_events() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let (ui, history) = write_roo_task_fixture(
            source_home.path(),
            ClientId::RooCode,
            "task",
            "old-model",
            "old-agent",
        );
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ui).unwrap()).unwrap();
        let mut later = original[0].clone();
        later["ts"] = serde_json::json!("2026-06-25T10:00:01Z");
        std::fs::write(
            &ui,
            serde_json::to_vec(&[original[0].clone(), later.clone()]).unwrap(),
        )
        .unwrap();
        let run = || {
            with_isolated_tokscale_cache(cache_home.path(), || {
                parse_all_messages_with_pricing_with_env_strategy(
                    source_home.path().to_str().unwrap(),
                    &["roocode".to_string()],
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                    None,
                )
            })
        };
        let collected = run();
        assert_eq!(collected.len(), 2);
        let latest = collected.iter().map(|row| row.timestamp).max().unwrap();
        std::fs::write(&ui, serde_json::to_vec(&[later]).unwrap()).unwrap();
        assert_eq!(run().len(), 2);
        write_roo_history_fixture(&history, "new-model", "new-agent");
        let annotated = run();
        assert_eq!(annotated.len(), 2);
        assert_eq!(
            annotated.iter().map(|row| row.tokens.input).sum::<i64>(),
            200
        );
        let archived = annotated
            .iter()
            .find(|row| row.timestamp != latest)
            .unwrap();
        assert_eq!(archived.model_id, "old-model");
        let live = annotated
            .iter()
            .find(|row| row.timestamp == latest)
            .unwrap();
        assert_eq!(live.model_id, "new-model");
        assert_eq!(live.agent.as_deref(), Some("new-agent"));
        std::fs::remove_dir_all(ui.parent().unwrap()).unwrap();
        assert_eq!(run(), annotated);
    }

    #[test]
    #[serial_test::serial]
    fn test_roo_family_history_rewrite_refreshes_materialized_and_streaming_caches() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let family = [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline];
        let mut litellm = HashMap::new();
        litellm.insert(
            "old-model".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                ..Default::default()
            },
        );
        litellm.insert(
            "new-model-with-longer-id".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.01),
                output_cost_per_token: Some(0.02),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let mut histories = Vec::new();
        for client in family {
            let task_id = format!("{}-task", client.as_str());
            let (_, history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &task_id,
                "old-model",
                "old-agent",
            );
            histories.push(history);
        }

        let home = source_home.path().to_str().unwrap().to_string();
        let clients: Vec<String> = family
            .into_iter()
            .map(|client| client.as_str().to_string())
            .collect();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    None,
                )
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_: &UnifiedMessage| true,
                    &mut |message: &UnifiedMessage| messages.push(message.clone()),
                );
                messages
            })
        };

        let materialized_before = run_materialized();
        let streaming_before = run_streaming();
        for (lane, messages) in [
            ("materialized", &materialized_before),
            ("streaming", &streaming_before),
        ] {
            assert_eq!(messages.len(), 3, "{lane} seed message count");
            assert!(messages.iter().all(|message| {
                message.model_id == "old-model" && message.agent.as_deref() == Some("old-agent")
            }));
        }

        for history in &histories {
            write_roo_history_fixture(history, "new-model-with-longer-id", "new-agent");
        }

        let materialized_after = run_materialized();
        let streaming_after = run_streaming();
        assert_eq!(materialized_after.len(), 3);
        assert_eq!(streaming_after.len(), 3);
        for client in family {
            let client_name = client.as_str();
            let materialized = materialized_after
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();
            let streaming = streaming_after
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();
            let materialized_before = materialized_before
                .iter()
                .find(|message| message.client == client_name)
                .unwrap();

            assert_eq!(materialized.session_id, format!("{client_name}-task"));
            assert_eq!(materialized.model_id, "new-model-with-longer-id");
            assert_eq!(materialized.agent.as_deref(), Some("new-agent"));
            assert_eq!(materialized.tokens.input, 100);
            assert_eq!(materialized.tokens.output, 25);
            assert_eq!(materialized.tokens.cache_read, 10);
            assert_eq!(materialized.tokens.cache_write, 5);
            assert!(
                materialized.cost > materialized_before.cost,
                "{client_name} history model rewrite must refresh derived pricing"
            );

            assert_eq!(streaming.client, materialized.client);
            assert_eq!(streaming.session_id, materialized.session_id);
            assert_eq!(streaming.model_id, materialized.model_id);
            assert_eq!(streaming.agent, materialized.agent);
            assert_eq!(streaming.tokens.input, materialized.tokens.input);
            assert_eq!(streaming.tokens.output, materialized.tokens.output);
            assert_eq!(streaming.tokens.cache_read, materialized.tokens.cache_read);
            assert_eq!(
                streaming.tokens.cache_write,
                materialized.tokens.cache_write
            );
            assert!((streaming.cost - materialized.cost).abs() < 1e-9);
        }
    }

    fn latest_source_mtime_probes_roo_history(client: ClientId) {
        let source_home = tempfile::TempDir::new().unwrap();
        let (ui_messages, history) = write_roo_task_fixture(
            source_home.path(),
            client,
            "mtime-task",
            "test-model",
            "test-agent",
        );
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for (path, time) in [(&ui_messages, stale_time), (&history, fresh_time)] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(time) else {
                return;
            };
        }

        let token = latest_source_mtime_ms(&LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec![client.as_str().to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            token,
            1_700_086_400_000,
            "{} change token must include the history sibling",
            client.as_str()
        );
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_roocode_history() {
        latest_source_mtime_probes_roo_history(ClientId::RooCode);
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_kilocode_history() {
        latest_source_mtime_probes_roo_history(ClientId::KiloCode);
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_cline_history() {
        latest_source_mtime_probes_roo_history(ClientId::Cline);
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_keeps_roo_family_with_fresh_history() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let family = [ClientId::RooCode, ClientId::KiloCode, ClientId::Cline];
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);

        for client in family {
            let stale_id = format!("{}-stale", client.as_str());
            let active_id = format!("{}-active", client.as_str());
            let (stale_ui, stale_history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &stale_id,
                "stale-model",
                "stale-agent",
            );
            let (active_ui, active_history) = write_roo_task_fixture(
                source_home.path(),
                client,
                &active_id,
                "active-model",
                "active-agent",
            );
            for path in [&stale_ui, &stale_history, &active_ui, &active_history] {
                let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
                let Ok(()) = file.set_modified(stale_time) else {
                    return;
                };
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&active_history)
                .unwrap();
            let Ok(()) = file.set_modified(fresh_time) else {
                return;
            };
        }

        let clients: Vec<String> = family
            .into_iter()
            .map(|client| client.as_str().to_string())
            .collect();
        let parsed = with_isolated_tokscale_cache(cache_home.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(clients),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(1_700_043_200_000),
            })
            .unwrap()
        });

        assert_eq!(parsed.messages.len(), 3);
        for client in family {
            assert_eq!(parsed.counts.get(client), 1);
            assert!(parsed.messages.iter().any(|message| {
                message.client == client.as_str()
                    && message.session_id == format!("{}-active", client.as_str())
                    && message.model_id == "active-model"
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_modified_after_roo_history_stat_failure_keeps_source() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let ui_messages = temp_dir.path().join("ui_messages.json");
        std::fs::write(&ui_messages, b"[]").unwrap();
        let history = sessions::roocode::history_path_for_ui_messages(&ui_messages);
        std::os::unix::fs::symlink("api_conversation_history.json", &history).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::RooCode)
            .push(ui_messages.clone());
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::RooCode),
            std::slice::from_ref(&ui_messages),
            "an unreadable history sibling must fail open during pruning"
        );
    }

    struct M13RelatedFixture {
        droid_source: PathBuf,
        droid_related: PathBuf,
        kimi_source: PathBuf,
        kimi_related: PathBuf,
        kiro_source: PathBuf,
        kiro_related: PathBuf,
    }

    fn write_m13_primary_fixtures(home: &Path) -> M13RelatedFixture {
        let droid_dir = home.join(".factory/sessions");
        std::fs::create_dir_all(&droid_dir).unwrap();
        let droid_source = droid_dir.join("droid-session.settings.json");
        std::fs::write(
            &droid_source,
            r#"{"providerLock":"anthropic","providerLockTimestamp":"2026-01-01T00:00:00Z","tokenUsage":{"inputTokens":100,"outputTokens":20,"cacheCreationTokens":5,"cacheReadTokens":10,"thinkingTokens":2}}"#,
        )
        .unwrap();
        let droid_related = droid_dir.join("droid-session.jsonl");

        let kimi_session_dir = home.join(".kimi/sessions/group-1/kimi-session");
        std::fs::create_dir_all(&kimi_session_dir).unwrap();
        let kimi_source = kimi_session_dir.join("wire.jsonl");
        std::fs::write(
            &kimi_source,
            r#"{"timestamp":1767225600.0,"message":{"type":"StatusUpdate","payload":{"token_usage":{"input_other":50,"output":5,"input_cache_read":3,"input_cache_creation":2},"message_id":"kimi-turn"}}}"#,
        )
        .unwrap();
        let kimi_related = home.join(".kimi/config.json");

        let kiro_dir = home.join(".kiro/sessions/cli");
        std::fs::create_dir_all(&kiro_dir).unwrap();
        let kiro_source = kiro_dir.join("kiro-session.json");
        std::fs::write(
            &kiro_source,
            r#"{"session_id":"kiro-session","cwd":"/tmp/m13-project","session_state":{"rts_model_state":{"model_info":{"model_id":"kiro-model","context_window_tokens":1000}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"end_timestamp":1767225601,"total_request_count":1,"message_ids":["kiro-turn"],"context_usage_percentage":10.0}]}}}"#,
        )
        .unwrap();
        let kiro_related = kiro_source.with_extension("jsonl");

        M13RelatedFixture {
            droid_source,
            droid_related,
            kimi_source,
            kimi_related,
            kiro_source,
            kiro_related,
        }
    }

    fn write_m13_related_fixtures(paths: &M13RelatedFixture) {
        std::fs::write(
            &paths.droid_related,
            r#"{"message":"Model: Claude-Opus-4.5-[Anthropic]"}"#,
        )
        .unwrap();
        std::fs::write(&paths.kimi_related, r#"{"model":"kimi-new-model"}"#).unwrap();
        std::fs::write(
            &paths.kiro_related,
            concat!(
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"kiro-prompt","content":[{"kind":"text","data":"prompt body"}],"meta":{"timestamp":1767225600.0}}}"#,
                "\n",
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"kiro-turn","content":[{"kind":"text","data":"abcdefghijklmnop"}]}}"#,
                "\n"
            ),
        )
        .unwrap();
    }

    fn set_m13_fixture_mtimes(
        paths: &M13RelatedFixture,
        primary_time: std::time::SystemTime,
        related_time: std::time::SystemTime,
    ) -> bool {
        for path in [&paths.droid_source, &paths.kimi_source, &paths.kiro_source] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(primary_time).is_err() {
                return false;
            }
        }
        for path in [
            &paths.droid_related,
            &paths.kimi_related,
            &paths.kiro_related,
        ] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(related_time).is_err() {
                return false;
            }
        }
        true
    }

    #[test]
    #[serial_test::serial]
    fn test_m13_related_sources_refresh_materialized_and_streaming_caches() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        let clients = vec!["droid".to_string(), "kimi".to_string(), "kiro".to_string()];

        let mut litellm = HashMap::new();
        for (model, input_rate, output_rate) in [
            ("claude-unknown", 0.001, 0.002),
            ("claude-opus-4-5", 0.01, 0.02),
            ("kimi-for-coding", 0.001, 0.002),
            ("kimi-new-model", 0.01, 0.02),
            ("kiro-model", 0.003, 0.004),
        ] {
            litellm.insert(
                model.to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(input_rate),
                    output_cost_per_token: Some(output_rate),
                    ..Default::default()
                },
            );
        }
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let home = source_home.path().to_str().unwrap().to_string();
        let run_materialized = || {
            with_isolated_tokscale_cache(materialized_cache.path(), || {
                parse_all_messages_with_pricing_with_env_strategy(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    None,
                )
            })
        };
        let run_streaming = || {
            with_isolated_tokscale_cache(streaming_cache.path(), || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    &home,
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_: &UnifiedMessage| true,
                    &mut |message: &UnifiedMessage| messages.push(message.clone()),
                );
                messages
            })
        };

        let materialized_before = run_materialized();
        let streaming_before = run_streaming();
        assert_eq!(materialized_before.len(), 3);
        assert_eq!(streaming_before.len(), 3);
        for (client, model) in [
            ("droid", "claude-unknown"),
            ("kimi", "kimi-for-coding"),
            ("kiro", "kiro-model"),
        ] {
            assert_eq!(
                materialized_before
                    .iter()
                    .find(|message| message.client == client)
                    .unwrap()
                    .model_id,
                model
            );
            assert_eq!(
                streaming_before
                    .iter()
                    .find(|message| message.client == client)
                    .unwrap()
                    .model_id,
                model
            );
        }

        write_m13_related_fixtures(&paths);

        let materialized_after = run_materialized();
        let streaming_after = run_streaming();
        assert_eq!(materialized_after.len(), 3);
        assert_eq!(streaming_after.len(), 3);
        for client in ["droid", "kimi", "kiro"] {
            let before = materialized_before
                .iter()
                .find(|message| message.client == client)
                .unwrap();
            let materialized = materialized_after
                .iter()
                .find(|message| message.client == client)
                .unwrap();
            let streaming = streaming_after
                .iter()
                .find(|message| message.client == client)
                .unwrap();

            let expected_model = match client {
                "droid" => "claude-opus-4-5",
                "kimi" => "kimi-new-model",
                "kiro" => "kiro-model",
                _ => unreachable!(),
            };
            assert_eq!(materialized.model_id, expected_model);
            assert!(
                materialized.cost > before.cost,
                "{client} related-source creation must refresh derived cost"
            );
            if client == "kiro" {
                assert_eq!(before.tokens.output, 0);
                assert_eq!(materialized.tokens.output, 4);
                assert_eq!(materialized.timestamp, 1_767_225_600_000);
                assert_eq!(materialized.duration_ms, Some(1_000));
            }

            assert_eq!(streaming.client, materialized.client);
            assert_eq!(streaming.session_id, materialized.session_id);
            assert_eq!(streaming.model_id, materialized.model_id);
            assert_eq!(streaming.provider_id, materialized.provider_id);
            assert_eq!(streaming.workspace_key, materialized.workspace_key);
            assert_eq!(streaming.workspace_label, materialized.workspace_label);
            assert_eq!(streaming.timestamp, materialized.timestamp);
            assert_eq!(streaming.duration_ms, materialized.duration_ms);
            assert_eq!(streaming.message_count, materialized.message_count);
            assert_eq!(streaming.tokens.input, materialized.tokens.input);
            assert_eq!(streaming.tokens.output, materialized.tokens.output);
            assert_eq!(streaming.tokens.cache_read, materialized.tokens.cache_read);
            assert_eq!(
                streaming.tokens.cache_write,
                materialized.tokens.cache_write
            );
            assert_eq!(streaming.tokens.reasoning, materialized.tokens.reasoning);
            assert!((streaming.cost - materialized.cost).abs() < 1e-9);
        }
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_m13_related_sources() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        write_m13_related_fixtures(&paths);
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        if !set_m13_fixture_mtimes(&paths, stale_time, fresh_time) {
            return;
        }

        for client in [ClientId::Droid, ClientId::Kimi, ClientId::Kiro] {
            let token = latest_source_mtime_ms(&LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec![client.as_str().to_string()]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: None,
            })
            .unwrap();
            assert_eq!(
                token,
                1_700_086_400_000,
                "{} change token must include its parser dependency",
                client.as_str()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_keeps_m13_sources_with_fresh_dependencies() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        write_m13_related_fixtures(&paths);
        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        if !set_m13_fixture_mtimes(&paths, stale_time, fresh_time) {
            return;
        }

        let parsed = with_isolated_tokscale_cache(cache_home.path(), || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec![
                    "droid".to_string(),
                    "kimi".to_string(),
                    "kiro".to_string(),
                ]),
                since: None,
                until: None,
                year: None,
                scanner_settings: scanner::ScannerSettings::default(),
                modified_after: Some(1_700_043_200_000),
            })
            .unwrap()
        });

        assert_eq!(parsed.messages.len(), 3);
        assert_eq!(parsed.counts.get(ClientId::Droid), 1);
        assert_eq!(parsed.counts.get(ClientId::Kimi), 1);
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);
        assert!(parsed
            .messages
            .iter()
            .any(|message| { message.client == "droid" && message.model_id == "claude-opus-4-5" }));
        assert!(parsed
            .messages
            .iter()
            .any(|message| { message.client == "kimi" && message.model_id == "kimi-new-model" }));
        assert!(parsed.messages.iter().any(|message| {
            message.client == "kiro" && message.output == 4 && message.duration_ms == Some(1_000)
        }));
    }

    #[test]
    fn test_modified_after_prunes_stale_m13_sources_without_dependencies() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Droid)
            .push(paths.droid_source);
        scan_result.get_mut(ClientId::Kimi).push(paths.kimi_source);
        scan_result.get_mut(ClientId::Kiro).push(paths.kiro_source);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert!(scan_result.get(ClientId::Droid).is_empty());
        assert!(scan_result.get(ClientId::Kimi).is_empty());
        assert!(scan_result.get(ClientId::Kiro).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_modified_after_m13_dependency_stat_failures_keep_sources() {
        let source_home = tempfile::TempDir::new().unwrap();
        let paths = write_m13_primary_fixtures(source_home.path());
        for related in [
            &paths.droid_related,
            &paths.kimi_related,
            &paths.kiro_related,
        ] {
            std::os::unix::fs::symlink(related.file_name().unwrap(), related).unwrap();
        }

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Droid)
            .push(paths.droid_source.clone());
        scan_result
            .get_mut(ClientId::Kimi)
            .push(paths.kimi_source.clone());
        scan_result
            .get_mut(ClientId::Kiro)
            .push(paths.kiro_source.clone());
        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::Droid),
            std::slice::from_ref(&paths.droid_source)
        );
        assert_eq!(
            scan_result.get(ClientId::Kimi),
            std::slice::from_ref(&paths.kimi_source)
        );
        assert_eq!(
            scan_result.get(ClientId::Kiro),
            std::slice::from_ref(&paths.kiro_source)
        );
    }

    // micode `.db` is WAL-mode SQLite reached via the generic `*.db` glob, so it
    // must be exempt from mtime pruning (a WAL-only write leaves the main db's
    // mtime untouched) — same treatment as Hermes / Zed. Antigravity CLI is instead
    // pruned by its db and `-wal` mtimes, except symlinked dbs and on Windows
    // (`antigravity_cli_db_mtime_ms`, `prune_scan_result_by_mtime`).
    #[test]
    fn test_modified_after_never_prunes_micode_dbs() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let micode_db = temp_dir.path().join("micode.db");
        std::fs::File::create(&micode_db).unwrap();
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::MiMoCode)
            .push(micode_db.clone());
        scan_result
            .get_mut(ClientId::Claude)
            .push(claude_log.clone());

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        crate::prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::MiMoCode),
            std::slice::from_ref(&micode_db),
            "micode .db (a WAL-mode SQLite source) must survive mtime pruning"
        );
        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "a plain-file client's stale log is still pruned"
        );
    }

    // gjc (`$GJC_CODING_AGENT_DIR/sessions/*.jsonl`) must be discovered via the
    // EnvVar fallback root (`.gjc/agent`) + `*.jsonl` glob and flow through the
    // streaming lane, keeping its authoritative embedded `usage.cost.total`
    // (A1). With pricing absent the guard's reprice branch is a no-op; the
    // materialized path mirrors upstream's proven reprice-when-absent guard.
    #[test]
    #[serial_test::serial]
    fn test_streaming_gjc_flows_with_authoritative_cost() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let gjc_dir = source_home.path().join(".gjc/agent/sessions");
            std::fs::create_dir_all(&gjc_dir).unwrap();
            std::fs::write(
                gjc_dir.join("test.jsonl"),
                "{\"type\":\"session\",\"id\":\"gjc_ses_001\",\"cwd\":\"/work/pi\"}\n{\"type\":\"message\",\"id\":\"msg_001\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-sonnet-4\",\"provider\":\"anthropic\",\"timestamp\":1767225601000,\"usage\":{\"input\":100,\"output\":50,\"cost\":{\"total\":0.3}}}}\n",
            )
            .unwrap();

            let mut cost_sum = 0.0f64;
            let mut count = 0usize;
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &["gjc".to_string()],
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| {
                    cost_sum += m.cost;
                    count += 1;
                },
            );

            assert_eq!(
                count, 1,
                "the gjc assistant message must flow through the streaming lane"
            );
            assert!(
                (cost_sum - 0.3).abs() < 1e-9,
                "authoritative gjc cost must reach the sink (got {cost_sum})"
            );
        }
    }

    // jcode's `session_*.json` snapshot is a file-lane source whose sibling
    // `.journal.jsonl` is appended between snapshot rewrites without touching
    // the snapshot mtime, so it must be exempt from mtime pruning like the WAL
    // db lanes — otherwise an active session with a stale snapshot is dropped
    // and its recent journal turns vanish from the live tail.
    #[test]
    fn test_modified_after_never_prunes_jcode_sessions() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let jcode_snapshot = temp_dir.path().join("session_x.json");
        std::fs::File::create(&jcode_snapshot).unwrap();
        let claude_log = temp_dir.path().join("session.jsonl");
        std::fs::File::create(&claude_log).unwrap();

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Jcode)
            .push(jcode_snapshot.clone());
        scan_result
            .get_mut(ClientId::Claude)
            .push(claude_log.clone());

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 3_600_000;
        crate::prune_scan_result_by_mtime(&mut scan_result, future_ms);

        assert_eq!(
            scan_result.get(ClientId::Jcode),
            std::slice::from_ref(&jcode_snapshot),
            "jcode snapshot (its journal sibling can change without it) must survive mtime pruning"
        );
        assert!(
            scan_result.get(ClientId::Claude).is_empty(),
            "a plain-file client's stale log is still pruned"
        );
    }

    #[test]
    fn test_modified_after_prunes_grok_by_updates_or_signals_mtime() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir_all(&active_dir).unwrap();

        let stale_updates = stale_dir.join("updates.jsonl");
        let stale_signals = stale_dir.join("signals.json");
        let active_updates = active_dir.join("updates.jsonl");
        let active_signals = active_dir.join("signals.json");
        for path in [
            &stale_updates,
            &stale_signals,
            &active_updates,
            &active_signals,
        ] {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&stale_updates, &stale_signals, &active_updates] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_signals)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Grok)
            .extend([stale_updates.clone(), active_updates.clone()]);

        crate::prune_scan_result_by_mtime(&mut scan_result, 1_700_043_200_000);

        assert_eq!(
            scan_result.get(ClientId::Grok),
            std::slice::from_ref(&active_updates),
            "stale Grok sessions should be pruned while a fresh signals sibling keeps its session"
        );
    }

    #[test]
    fn test_modified_after_retains_grok_authority_sources() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        let logs_dir = temp_dir.path().join("logs");
        for dir in [&stale_dir, &active_dir, &logs_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let stale_updates = stale_dir.join("updates.jsonl");
        let active_updates = active_dir.join("updates.jsonl");
        let unified = logs_dir.join("unified.jsonl");
        for path in [&stale_updates, &active_updates, &unified] {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&stale_updates, &unified] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_updates)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut legacy_fresh = scanner::ScanResult::default();
        legacy_fresh.get_mut(ClientId::Grok).extend([
            stale_updates.clone(),
            active_updates.clone(),
            unified.clone(),
        ]);
        crate::prune_scan_result_by_mtime(&mut legacy_fresh, 1_700_043_200_000);
        assert_eq!(
            legacy_fresh.get(ClientId::Grok),
            &[active_updates.clone(), unified.clone()],
            "a stale unified authority source must survive while a legacy source is fresh"
        );

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&unified)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_updates)
            .unwrap();
        let Ok(()) = file.set_modified(stale_time) else {
            return;
        };

        let mut unified_fresh = scanner::ScanResult::default();
        unified_fresh.get_mut(ClientId::Grok).extend([
            stale_updates.clone(),
            active_updates.clone(),
            unified.clone(),
        ]);
        crate::prune_scan_result_by_mtime(&mut unified_fresh, 1_700_043_200_000);
        assert_eq!(
            unified_fresh.get(ClientId::Grok),
            &[stale_updates, active_updates, unified],
            "a fresh unified source needs the legacy cohort for workspace attribution"
        );
    }

    // The pruning helper must treat a fresh write to *any* metadata sibling the
    // parser reads (not just signals.json) as source activity — otherwise a
    // summary.json- or events.jsonl-only write (a late-arriving model id) lets an
    // otherwise-stale session be pruned and its fresh model never re-parsed.
    fn prune_grok_keeps_session_with_fresh_sibling(fresh_sibling: &str) {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let stale_dir = temp_dir.path().join("stale");
        let active_dir = temp_dir.path().join("active");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::create_dir_all(&active_dir).unwrap();

        let stale_updates = stale_dir.join("updates.jsonl");
        let active_updates = active_dir.join("updates.jsonl");
        let active_fresh = active_dir.join(fresh_sibling);
        // Every session file starts stale; only the one fresh sibling moves.
        let mut all_paths = vec![stale_updates.clone(), active_updates.clone()];
        for name in message_cache::GROK_METADATA_SIBLINGS {
            all_paths.push(stale_dir.join(name));
            all_paths.push(active_dir.join(name));
        }
        for path in &all_paths {
            std::fs::File::create(path).unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let active_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in &all_paths {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let Ok(()) = file.set_modified(stale_time) else {
                return;
            };
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&active_fresh)
            .unwrap();
        let Ok(()) = file.set_modified(active_time) else {
            return;
        };

        let mut scan_result = scanner::ScanResult::default();
        scan_result
            .get_mut(ClientId::Grok)
            .extend([stale_updates.clone(), active_updates.clone()]);

        crate::prune_scan_result_by_mtime(&mut scan_result, 1_700_043_200_000);

        assert_eq!(
            scan_result.get(ClientId::Grok),
            std::slice::from_ref(&active_updates),
            "a fresh {fresh_sibling} sibling must keep its otherwise-stale Grok session"
        );
    }

    #[test]
    fn test_modified_after_prunes_grok_keeps_session_with_fresh_summary() {
        prune_grok_keeps_session_with_fresh_sibling("summary.json");
    }

    #[test]
    fn test_modified_after_prunes_grok_keeps_session_with_fresh_events() {
        prune_grok_keeps_session_with_fresh_sibling("events.jsonl");
    }

    // The live-tail change token must move when Grok rewrites *any* metadata
    // sibling the parser reads, even though updates.jsonl is unchanged; otherwise
    // UsageTail short-circuits and the session stays pinned to its fallback model.
    fn latest_source_mtime_ms_probes_grok_sibling(fresh_sibling: &str) {
        let source_home = tempfile::TempDir::new().unwrap();
        let session_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/session-uuid-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        let updates = session_dir.join("updates.jsonl");
        std::fs::write(&updates, b"{\"totalTokens\":1}\n").unwrap();
        // Every sibling exists and is stale; only the target sibling is newer.
        for name in message_cache::GROK_METADATA_SIBLINGS {
            std::fs::write(session_dir.join(name), b"{}").unwrap();
        }

        let stale_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let fresh_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for name in std::iter::once("updates.jsonl").chain(message_cache::GROK_METADATA_SIBLINGS) {
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(session_dir.join(name))
                .unwrap();
            let Ok(()) = f.set_modified(stale_time) else {
                return;
            };
        }
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(session_dir.join(fresh_sibling))
            .unwrap();
        let Ok(()) = f.set_modified(fresh_time) else {
            return;
        };

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["grok".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must reflect the fresh {fresh_sibling} mtime, not just updates.jsonl"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_grok_summary() {
        latest_source_mtime_ms_probes_grok_sibling("summary.json");
    }

    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_grok_events() {
        latest_source_mtime_ms_probes_grok_sibling("events.jsonl");
    }

    #[test]
    #[serial_test::serial]
    fn grok_streaming_cache_hits_refresh_derived_date_before_filtering() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let unified = logs_dir.join("unified.jsonl");
        std::fs::write(
            &unified,
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"cached\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":10,\"completion_tokens\":2}}\n",
        )
        .unwrap();

        let mut cached_messages = sessions::grok::parse_grok_unified_log_file(&unified);
        assert_eq!(cached_messages.len(), 1);
        let expected_date = cached_messages[0].date.clone();
        cached_messages[0].date = "stale-cached-date".to_string();
        let fingerprint = message_cache::SourceFingerprint::from_grok_path(&unified).unwrap();
        let mut cache = message_cache::SourceMessageCache::load();
        cache.insert(message_cache::CachedSourceEntry::new(
            message_cache::CacheIdentity::for_client(ClientId::Grok),
            &unified,
            fingerprint,
            cached_messages,
            Vec::new(),
            None,
        ));
        cache.save_if_dirty();

        let clients = vec!["grok".to_string()];
        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|message| message.date == expected_date,
            &mut |message| streamed.push(message.clone()),
        );

        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].date, expected_date);
    }

    #[test]
    #[serial_test::serial]
    fn grok_materialized_reprices_after_legacy_model_carry_over() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let covered_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/covered");
        std::fs::create_dir_all(&covered_dir).unwrap();
        std::fs::write(
            covered_dir.join("updates.jsonl"),
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000000000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":999,\"agentTimestampMs\":1700000001000}}}\n",
            ),
        )
        .unwrap();
        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        std::fs::write(
            logs_dir.join("unified.jsonl"),
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"covered\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":100,\"cached_prompt_tokens\":60,\"completion_tokens\":25,\"reasoning_tokens\":5}}\n",
        )
        .unwrap();

        let mut litellm = HashMap::new();
        litellm.insert(
            "grok-build".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_read_input_token_cost: Some(0.0005),
                ..Default::default()
            },
        );
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let clients = vec!["grok".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        let materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                Some(&pricing),
                false,
                &scanner_settings,
                None,
            )
        };
        let streamed = || {
            let mut messages = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                Some(&pricing),
                false,
                &scanner_settings,
                &|_| true,
                &mut |message| messages.push(message.clone()),
            );
            messages
        };

        let cold = materialized();
        assert_eq!(cold.len(), 1);
        assert_eq!(cold[0].model_id, "grok-build");
        let expected_cost = pricing.calculate_cost_with_provider(
            &cold[0].model_id,
            Some(&cold[0].provider_id),
            &cold[0].tokens,
        );
        assert!(expected_cost > 0.0);
        assert_eq!(cold[0].cost, expected_cost);
        assert_eq!(cold[0].cost_source, CostSource::Estimated);
        assert_eq!(streamed(), cold);
        assert_eq!(materialized(), cold, "warm materialized cache must reprice");
        assert_eq!(streamed(), cold, "warm streaming cache must stay in parity");
    }

    #[test]
    #[serial_test::serial]
    fn grok_scoped_attribution_keeps_cache_lane_and_pricing_boundaries() {
        let source_home = tempfile::TempDir::new().unwrap();
        let materialized_cache = tempfile::TempDir::new().unwrap();
        let streaming_cache = tempfile::TempDir::new().unwrap();
        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let unified = logs_dir.join("unified.jsonl");
        std::fs::write(
            &unified,
            concat!(
                "{\"ts\":\"2026-07-31T00:00:00Z\",\"pid\":7,\"msg\":\"subagent read parent config (live)\",\"ctx\":{\"session_model_id\":\"grok-4.6\"}}\n",
                "{\"ts\":\"2026-07-31T00:00:01Z\",\"pid\":7,\"sid\":\"parent\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":10,\"cached_prompt_tokens\":2,\"completion_tokens\":4,\"reasoning_tokens\":1}}\n",
                "{\"ts\":\"2026-07-31T00:00:02Z\",\"pid\":7,\"msg\":\"subagent spawn credentials\",\"ctx\":{\"subagent_id\":\"child-a\",\"effective_model\":\"grok-4.7\",\"parent_model\":\"grok-4.6\"}}\n",
                "{\"ts\":\"2026-07-31T00:00:03Z\",\"pid\":7,\"sid\":\"child-a\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":11,\"completion_tokens\":2}}\n",
                "{\"ts\":\"2026-07-31T00:00:04Z\",\"pid\":7,\"msg\":\"subagent spawn credentials\",\"ctx\":{\"subagent_id\":\"child-b\",\"effective_model\":\"grok-4.8\",\"parent_model\":\"grok-4.6\"}}\n",
                "{\"ts\":\"2026-07-31T00:00:05Z\",\"pid\":7,\"sid\":\"child-b\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":12,\"completion_tokens\":3,\"reasoning_tokens\":1}}\n",
                "{\"ts\":\"2026-07-31T00:00:06Z\",\"pid\":7,\"sid\":\"child-retro\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":13,\"cached_prompt_tokens\":3,\"completion_tokens\":5,\"reasoning_tokens\":2}}\n",
                "{\"ts\":\"2026-07-31T00:00:07Z\",\"pid\":7,\"msg\":\"subagent completed\",\"ctx\":{\"subagent_id\":\"child-retro\",\"effective_model\":\"grok-4.9\"}}\n",
            ),
        )
        .unwrap();

        let provider_dir = source_home
            .path()
            .join(".grok/sessions/%2Fworkspace/provider");
        std::fs::create_dir_all(&provider_dir).unwrap();
        let provider_updates = provider_dir.join("updates.jsonl");
        std::fs::write(
            &provider_updates,
            "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"provider\",\"update\":{\"sessionUpdate\":\"turn_completed\",\"prompt_id\":\"provider-turn\",\"usage\":{\"inputTokens\":20,\"outputTokens\":5,\"cachedReadTokens\":4,\"reasoningTokens\":1,\"costUsdTicks\":1250000000,\"modelUsage\":{\"grok-provider\":{\"inputTokens\":20,\"outputTokens\":5,\"cachedReadTokens\":4,\"reasoningTokens\":1,\"costUsdTicks\":1250000000}}}},\"_meta\":{\"agentTimestampMs\":1785456010000}}}\n",
        )
        .unwrap();

        let raw_unified = sessions::grok::parse_grok_unified_log_file(&unified);
        assert_eq!(raw_unified.len(), 4);
        assert_eq!(
            raw_unified
                .iter()
                .map(|message| message.model_id.as_str())
                .collect::<Vec<_>>(),
            ["grok-4.6", "grok-4.7", "grok-4.8", "grok-4.9"]
        );
        assert!(raw_unified
            .iter()
            .all(|message| { message.cost == 0.0 && message.cost_source == CostSource::Unknown }));
        let provider_raw = sessions::grok::parse_grok_file(&provider_updates);
        assert_eq!(provider_raw.len(), 1);
        assert_eq!(provider_raw[0].cost, 1.25);
        assert_eq!(provider_raw[0].cost_source, CostSource::ProviderReported);

        let mut litellm = HashMap::new();
        for (model, input, output, cache_read) in [
            ("grok-4.6", 0.001, 0.002, 0.0005),
            ("grok-4.7", 0.003, 0.004, 0.0015),
            ("grok-4.8", 0.005, 0.006, 0.0025),
            ("grok-4.9", 0.007, 0.008, 0.0035),
            ("grok-provider", 1.0, 1.0, 1.0),
        ] {
            litellm.insert(
                model.to_string(),
                pricing::ModelPricing {
                    input_cost_per_token: Some(input),
                    output_cost_per_token: Some(output),
                    cache_read_input_token_cost: Some(cache_read),
                    ..Default::default()
                },
            );
        }
        let pricing = pricing::PricingService::new(litellm, HashMap::new());
        let expected_estimated_cost = raw_unified
            .iter()
            .map(|message| {
                pricing.calculate_cost_with_provider(
                    &message.model_id,
                    Some(&message.provider_id),
                    &message.tokens,
                )
            })
            .sum::<f64>();
        assert!(expected_estimated_cost > 0.0);

        let clients = vec!["grok".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        let sort_messages = |mut messages: Vec<UnifiedMessage>| {
            messages.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            messages
        };
        let materialized = |cache_home: &std::path::Path| {
            with_isolated_tokscale_cache(cache_home, || {
                sort_messages(parse_all_messages_with_pricing_with_env_strategy(
                    source_home.path().to_str().unwrap(),
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner_settings,
                    None,
                ))
            })
        };
        let streaming = |cache_home: &std::path::Path| {
            with_isolated_tokscale_cache(cache_home, || {
                let mut messages = Vec::new();
                scan_messages_streaming(
                    source_home.path().to_str().unwrap(),
                    &clients,
                    Some(&pricing),
                    false,
                    &scanner_settings,
                    &|_| true,
                    &mut |message| messages.push(message.clone()),
                );
                sort_messages(messages)
            })
        };

        let cold_materialized = materialized(materialized_cache.path());
        let cold_streaming = streaming(streaming_cache.path());
        assert_eq!(cold_materialized, cold_streaming);
        assert_eq!(materialized(materialized_cache.path()), cold_materialized);
        assert_eq!(streaming(streaming_cache.path()), cold_streaming);
        assert_eq!(cold_materialized.len(), 5);

        for raw in &raw_unified {
            let actual = cold_materialized
                .iter()
                .find(|message| message.dedup_key == raw.dedup_key)
                .unwrap();
            assert_eq!(actual.client, raw.client);
            assert_eq!(actual.model_id, raw.model_id);
            assert_eq!(actual.provider_id, raw.provider_id);
            assert_eq!(actual.session_id, raw.session_id);
            assert_eq!(actual.timestamp, raw.timestamp);
            assert_eq!(actual.date, raw.date);
            assert_eq!(actual.tokens, raw.tokens);
            assert_eq!(actual.message_count, raw.message_count);
            assert_eq!(actual.dedup_key, raw.dedup_key);
            assert_eq!(actual.is_turn_start, raw.is_turn_start);
            assert_eq!(actual.cost_source, CostSource::Estimated);
            assert_eq!(
                actual.cost,
                pricing.calculate_cost_with_provider(
                    &raw.model_id,
                    Some(&raw.provider_id),
                    &raw.tokens,
                )
            );
        }

        let provider = cold_materialized
            .iter()
            .find(|message| message.session_id == "provider")
            .unwrap();
        assert_eq!(provider.model_id, provider_raw[0].model_id);
        assert_eq!(provider.tokens, provider_raw[0].tokens);
        assert_eq!(provider.dedup_key, provider_raw[0].dedup_key);
        assert_eq!(provider.cost, provider_raw[0].cost);
        assert_eq!(provider.cost_source, CostSource::ProviderReported);
        let total_cost = cold_materialized
            .iter()
            .map(|message| message.cost)
            .sum::<f64>();
        assert!((total_cost - (1.25 + expected_estimated_cost)).abs() < 1e-12);

        let entries = aggregate_model_usage_entries(cold_materialized.clone(), &GroupBy::Model);
        let mut models = entries
            .iter()
            .map(|entry| entry.model.clone())
            .collect::<Vec<_>>();
        models.sort();
        assert_eq!(
            models,
            [
                "grok-4.6",
                "grok-4.7",
                "grok-4.8",
                "grok-4.9",
                "grok-provider",
            ]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.input).sum::<i64>(),
            cold_materialized
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>()
        );
        assert_eq!(
            entries.iter().map(|entry| entry.message_count).sum::<i32>(),
            cold_materialized
                .iter()
                .map(|message| message.message_count)
                .sum::<i32>()
        );
        assert!((entries.iter().map(|entry| entry.cost).sum::<f64>() - total_cost).abs() < 1e-12);
    }

    #[test]
    #[serial_test::serial]
    fn m17_grok_unified_precedence_tracks_source_lifecycle_across_all_lanes() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let clients = vec!["grok".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();

        let covered_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/covered");
        let legacy_only_dir = source_home
            .path()
            .join(".grok/sessions/%2Ftmp%2Fproject/legacy-only");
        std::fs::create_dir_all(&covered_dir).unwrap();
        std::fs::create_dir_all(&legacy_only_dir).unwrap();
        let covered_updates = covered_dir.join("updates.jsonl");
        let legacy_only_updates = legacy_only_dir.join("updates.jsonl");
        std::fs::write(
            &covered_updates,
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000000000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"covered\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":999,\"agentTimestampMs\":1700000001000}}}\n",
            ),
        )
        .unwrap();
        std::fs::write(
            &legacy_only_updates,
            concat!(
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"legacy-only\",\"update\":{\"sessionUpdate\":\"user_message_chunk\",\"_meta\":{\"modelId\":\"grok-build\"}},\"_meta\":{\"agentTimestampMs\":1700000010000}}}\n",
                "{\"method\":\"session/update\",\"params\":{\"sessionId\":\"legacy-only\",\"update\":{\"sessionUpdate\":\"agent_message_chunk\"},\"_meta\":{\"totalTokens\":17,\"agentTimestampMs\":1700000011000}}}\n",
            ),
        )
        .unwrap();
        let legacy_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        for path in [&covered_updates, &legacy_only_updates] {
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            if file.set_modified(legacy_time).is_err() {
                return;
            }
        }

        let local_options = || LocalParseOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner_settings.clone(),
            modified_after: None,
        };
        let report_options = || ReportOptions {
            home_dir: Some(source_home.path().to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            scanner_settings: scanner_settings.clone(),
            ..Default::default()
        };
        let sorted = |mut messages: Vec<UnifiedMessage>| {
            messages.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            messages
        };
        let materialized = || {
            sorted(parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
                None,
            ))
        };
        let streamed = || {
            let mut messages = Vec::new();
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
                &|_| true,
                &mut |message| messages.push(message.clone()),
            );
            sorted(messages)
        };

        let legacy = materialized();
        assert_eq!(legacy.len(), 2);
        assert_eq!(
            legacy
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            1_016
        );
        assert_eq!(materialized(), legacy, "legacy cache hits must stay stable");
        let before_unified = latest_source_mtime_ms(&local_options()).unwrap();
        let legacy_change_token = local_source_change_token(&local_options()).unwrap();
        assert_eq!(before_unified, 1_700_086_400_000);

        let logs_dir = source_home.path().join(".grok/logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let unified = logs_dir.join("unified.jsonl");
        std::fs::write(
            &unified,
            "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"covered\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":100,\"cached_prompt_tokens\":60,\"completion_tokens\":25,\"reasoning_tokens\":5}}\n",
        )
        .unwrap();
        let unified_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let unified_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&unified)
            .unwrap();
        if unified_file.set_modified(unified_time).is_err() {
            return;
        }
        assert_eq!(
            latest_source_mtime_ms(&local_options()).unwrap(),
            before_unified,
            "the newer legacy mtime deliberately masks unified topology changes"
        );
        let selected_change_token = local_source_change_token(&local_options()).unwrap();
        assert_ne!(selected_change_token, legacy_change_token);

        let selected = materialized();
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected
                .iter()
                .find(|message| message.session_id == "covered")
                .unwrap()
                .model_id,
            "grok-build"
        );
        let selected_tokens =
            selected
                .iter()
                .fold(TokenBreakdown::default(), |mut total, message| {
                    total.input += message.tokens.input;
                    total.output += message.tokens.output;
                    total.cache_read += message.tokens.cache_read;
                    total.cache_write += message.tokens.cache_write;
                    total.reasoning += message.tokens.reasoning;
                    total
                });
        assert_eq!(
            selected_tokens,
            TokenBreakdown {
                input: 57,
                output: 20,
                cache_read: 60,
                cache_write: 0,
                reasoning: 5,
                cache_write_1h: 0,
            }
        );
        assert!(selected.iter().any(|message| {
            message.session_id == "covered"
                && message
                    .dedup_key
                    .as_deref()
                    .is_some_and(|key| key.starts_with("grok-unified:"))
        }));
        assert!(selected
            .iter()
            .any(|message| message.session_id == "legacy-only"));
        assert_eq!(
            materialized(),
            selected,
            "selected cache hits must stay stable"
        );
        assert_eq!(streamed(), selected);

        let counted = parse_local_clients(local_options()).unwrap();
        assert_eq!(counted.counts.get(ClientId::Grok), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            57
        );
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.cache_read)
                .sum::<i64>(),
            60
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let assert_reports = |expected: (i32, i64, i64, i64, i64, i64)| {
            let (messages, input, output, cache_read, cache_write, reasoning) = expected;
            let model = runtime
                .block_on(get_model_report(report_options()))
                .unwrap();
            let monthly = runtime
                .block_on(get_monthly_report(report_options()))
                .unwrap();
            let hourly = runtime
                .block_on(get_hourly_report(report_options()))
                .unwrap();
            let agents = runtime
                .block_on(get_agents_report(report_options()))
                .unwrap();

            assert_eq!(
                (
                    model.total_messages,
                    model.total_input,
                    model.total_output,
                    model.total_cache_read,
                    model.total_cache_write,
                    model
                        .entries
                        .iter()
                        .map(|entry| entry.reasoning)
                        .sum::<i64>(),
                ),
                expected
            );
            assert_eq!(
                monthly
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.message_count,
                        totals.1 + entry.input,
                        totals.2 + entry.output,
                        totals.3 + entry.cache_read,
                        totals.4 + entry.cache_write,
                    ),),
                (messages, input, output, cache_read, cache_write)
            );
            assert_eq!(
                hourly
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.message_count,
                        totals.1 + entry.input,
                        totals.2 + entry.output,
                        totals.3 + entry.cache_read,
                        totals.4 + entry.cache_write,
                        totals.5 + entry.reasoning,
                    ),),
                expected
            );
            assert_eq!(
                agents
                    .entries
                    .iter()
                    .fold((0, 0, 0, 0, 0), |totals, entry| (
                        totals.0 + entry.input,
                        totals.1 + entry.output,
                        totals.2 + entry.cache_read,
                        totals.3 + entry.cache_write,
                        totals.4 + entry.reasoning,
                    ),),
                (input, output, cache_read, cache_write, reasoning)
            );
            assert_eq!(agents.total_messages, messages);
        };
        assert_reports((2, 57, 20, 60, 0, 5));
    }

    #[test]
    fn test_latest_source_mtime_ms_probes_auto_discovered_hermes_profile_wal() {
        let source_home = tempfile::TempDir::new().unwrap();
        let profile_dir = source_home.path().join(".hermes/profiles/research");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let db = profile_dir.join("state.db");
        let wal = profile_dir.join("state.db-wal");
        std::fs::File::create(&db).unwrap();
        std::fs::File::create(&wal).unwrap();

        let db_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let wal_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        let db_file = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        let Ok(()) = db_file.set_modified(db_time) else {
            return;
        };
        drop(db_file);
        let wal_file = std::fs::OpenOptions::new().write(true).open(&wal).unwrap();
        let Ok(()) = wal_file.set_modified(wal_time) else {
            return;
        };
        drop(wal_file);

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must include an auto-discovered profile WAL"
        );
    }

    // The live-tail change token must move when jcode appends to the sibling
    // `.journal.jsonl` even though the snapshot mtime is unchanged; otherwise
    // UsageTail short-circuits and never reflects the new turn.
    #[test]
    #[serial_test::serial]
    fn test_latest_source_mtime_ms_probes_jcode_journal() {
        let source_home = tempfile::TempDir::new().unwrap();
        let sessions_dir = source_home.path().join(".jcode/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let snapshot = sessions_dir.join("session_x.json");
        std::fs::write(&snapshot, br#"{"id":"session_x","messages":[]}"#).unwrap();
        let journal = sessions_dir.join("session_x.journal.jsonl");
        std::fs::write(&journal, b"{\"append_messages\":[]}\n").unwrap();

        // Snapshot old, journal strictly newer — the journal-only append the
        // probe must catch. Skip gracefully if the FS rejects set_modified.
        let snapshot_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let journal_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_086_400);
        let sf = std::fs::OpenOptions::new()
            .write(true)
            .open(&snapshot)
            .unwrap();
        let Ok(()) = sf.set_modified(snapshot_time) else {
            return;
        };
        drop(sf);
        let jf = std::fs::OpenOptions::new()
            .write(true)
            .open(&journal)
            .unwrap();
        let Ok(()) = jf.set_modified(journal_time) else {
            return;
        };
        drop(jf);

        let options = LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["jcode".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        };
        let token = crate::latest_source_mtime_ms(&options).unwrap();

        // The newer journal mtime must dominate; without the journal probe the
        // token would stop at the older snapshot mtime (1_700_000_000_000).
        assert_eq!(
            token, 1_700_086_400_000,
            "the change token must reflect the jcode journal mtime, not just the snapshot"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_honors_scanner_extra_scan_paths_for_zed_threads_db() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let extra_threads_dir = temp_dir.path().join("custom-zed/threads");
        std::fs::create_dir_all(&extra_threads_dir).unwrap();
        let threads_db = extra_threads_dir.join("threads.db");
        let conn = create_zed_sqlite_db(&threads_db);
        insert_zed_thread(&conn, "zed-extra-thread", "claude-sonnet-4-5");
        drop(conn);

        let parsed_default = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();
        assert_eq!(parsed_default.counts.get(ClientId::Zed), 0);
        assert!(parsed_default.messages.is_empty());

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("zed".to_string(), vec![extra_threads_dir]);
        let parsed_with_settings = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed_with_settings.counts.get(ClientId::Zed), 1);
        assert_eq!(parsed_with_settings.messages.len(), 1);
        assert_eq!(parsed_with_settings.messages[0].client, "zed");
        assert_eq!(
            parsed_with_settings.messages[0].session_id,
            "zed-extra-thread"
        );
        assert_eq!(
            parsed_with_settings.messages[0].model_id,
            "claude-sonnet-4-5"
        );
        assert_eq!(parsed_with_settings.messages[0].input, 42);
        assert_eq!(parsed_with_settings.messages[0].output, 7);
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_dedups_zed_threads_across_default_and_extra_dbs() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();

        // Place threads.db at the default platform path so the scanner finds it
        // as `zed_db` AND we also pass it via extraScanPaths.
        let default_threads_dir = temp_dir.path().join(".local/share/zed/threads");
        std::fs::create_dir_all(&default_threads_dir).unwrap();
        let default_db = default_threads_dir.join("threads.db");
        let conn = create_zed_sqlite_db(&default_db);
        insert_zed_thread(&conn, "shared-zed-thread", "claude-sonnet-4-5");
        drop(conn);

        // Point extraScanPaths.zed at the same directory — dedup should prevent
        // the thread from appearing twice.
        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert("zed".to_string(), vec![default_threads_dir.clone()]);
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        // Should see exactly 1 message, not 2 (deduped by canonicalize).
        assert_eq!(parsed.counts.get(ClientId::Zed), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].session_id, "shared-zed-thread");
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_zed_extra_scan_paths_nonexistent_dir_is_silent() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();

        let mut extra_scan_paths = std::collections::BTreeMap::new();
        extra_scan_paths.insert(
            "zed".to_string(),
            vec![temp_dir.path().join("does/not/exist")],
        );
        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zed".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                extra_scan_paths,
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Zed), 0);
        assert!(parsed.messages.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_dedups_default_and_auto_discovered_hermes_profile() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();

        let default_dir = temp_dir.path().join(".hermes");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_db = default_dir.join("state.db");
        let default_conn = create_hermes_sqlite_db(&default_db);
        insert_hermes_session(
            &default_conn,
            "shared-hermes-session",
            "claude-sonnet-4",
            2,
            100,
            25,
            0.07,
        );
        drop(default_conn);

        let profile_dir = temp_dir.path().join(".hermes/profiles/director_planning");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let profile_db = profile_dir.join("state.db");
        let profile_conn = create_hermes_sqlite_db(&profile_db);
        insert_hermes_session(
            &profile_conn,
            "shared-hermes-session",
            "claude-sonnet-4",
            9,
            999,
            999,
            9.99,
        );
        insert_hermes_session(
            &profile_conn,
            "profile-only-session",
            "claude-sonnet-4",
            1,
            30,
            3,
            0.02,
        );
        drop(profile_conn);

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["hermes".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Hermes), 3);
        assert_eq!(parsed.messages.len(), 2);
        let shared = parsed
            .messages
            .iter()
            .find(|message| message.session_id == "shared-hermes-session")
            .unwrap();
        assert_eq!(shared.input, 100);
        assert_eq!(shared.output, 25);
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.session_id == "profile-only-session"));
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_claude_filter_ignores_scanner_settings_opencode_db_paths() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        // Regression guard for the scanner client-filter bypass: even
        // when `scanner.opencodeDbPaths` pins an external opencode db,
        // a `--clients claude` request must NOT pull in OpenCode rows.
        // Before the fix, the merge ran outside the OpenCode-enabled
        // guard so user-pinned dbs leaked through both `messages` and
        // `counts` (the latter is computed before the message-level
        // client filter, so even the post-filter pipeline could not
        // hide a leaked count).
        let temp_dir = tempfile::TempDir::new().unwrap();

        // Claude session: one assistant message, the only thing the
        // filter should accept.
        let claude_dir = temp_dir.path().join(".claude/projects/myproject");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("conversation.jsonl"),
            r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
        )
        .unwrap();

        // External opencode.db that the user has pinned via
        // scanner.opencodeDbPaths. Without the fix, this would leak
        // into the Claude-only result.
        let outside_dir = temp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let external_db = outside_dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&external_db).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                "leaked-opencode",
                "should-not-show-up",
                r#"{
                    "role": "assistant",
                    "modelID": "claude-sonnet-4",
                    "providerID": "anthropic",
                    "tokens": { "input": 9999, "output": 9999, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
                    "time": { "created": 1700000000000.0 }
                }"#
            ],
        )
        .unwrap();
        drop(conn);

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings {
                opencode_db_paths: vec![external_db.clone()],
                ..Default::default()
            },
            modified_after: None,
        })
        .unwrap();

        assert_eq!(
            parsed.counts.get(ClientId::OpenCode),
            0,
            "OpenCode count must stay zero under a Claude-only filter even \
             when scanner.opencodeDbPaths is set"
        );
        assert_eq!(
            parsed.counts.get(ClientId::Claude),
            1,
            "Claude message must still be counted"
        );
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "claude");
        assert!(
            parsed.messages.iter().all(|m| m.client != "opencode"),
            "no OpenCode messages may leak into a Claude-only result, got {:?}",
            parsed.messages
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_claude_transcripts_count_only_usage_metadata() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let transcripts_dir = temp_dir.path().join(".claude/transcripts");
        std::fs::create_dir_all(&transcripts_dir).unwrap();
        std::fs::write(
            transcripts_dir.join("ses_123456789012345678901234567.jsonl"),
            r#"{"type":"user","timestamp":"2026-04-01T10:00:00.000Z","message":{"content":"Wrapped prompt"}}
{"type":"assistant","timestamp":"2026-04-01T10:00:01.000Z","requestId":"req_wrapper","message":{"id":"msg_wrapper","model":"claude-sonnet-4","usage":{"input_tokens":123,"output_tokens":45,"cache_read_input_tokens":67,"cache_creation_input_tokens":8}}}
"#,
        )
        .unwrap();
        std::fs::write(
            transcripts_dir.join("ses_765432109876543210987654321.jsonl"),
            r#"{"type":"user","timestamp":"2026-04-01T10:00:00.000Z","message":{"content":"Wrapped prompt"}}
{"type":"tool_use","timestamp":"2026-04-01T10:00:01.000Z","message":{"content":"Run tool"}}
{"type":"tool_result","timestamp":"2026-04-01T10:00:02.000Z","message":{"content":"Tool result"}}
"#,
        )
        .unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["claude".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Claude), 1);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].client, "claude");
        assert_eq!(
            parsed.messages[0].session_id,
            "ses_123456789012345678901234567"
        );
        assert_eq!(parsed.messages[0].model_id, "claude-sonnet-4");
        assert_eq!(parsed.messages[0].input, 123);
        assert_eq!(parsed.messages[0].output, 45);
        assert_eq!(parsed.messages[0].cache_read, 67);
        assert_eq!(parsed.messages[0].cache_write, 8);
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_refreshes_cc_mirror_provider_when_variant_metadata_changes() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let variant_dir = source_home.path().join(".cc-mirror/kimi-code");
            let config_dir = source_home.path().join("mirror-configs/kimi-code");
            let project_dir = config_dir.join("projects/project-one");
            std::fs::create_dir_all(&project_dir).unwrap();
            std::fs::create_dir_all(&variant_dir).unwrap();
            let variant_path = variant_dir.join("variant.json");
            std::fs::write(
                &variant_path,
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "kimi",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();
            let session_path = project_dir.join("session.jsonl");
            std::fs::write(
                &session_path,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
            )
            .unwrap();

            let first_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(first_messages.len(), 1);
            assert_eq!(first_messages[0].client, "cc-mirror/kimi-code");
            assert_eq!(first_messages[0].provider_id, "kimi");

            std::fs::write(
                &variant_path,
                serde_json::json!({
                    "name": "kimi-code",
                    "provider": "minimax",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();

            let refreshed_messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(refreshed_messages.len(), 1);
            assert_eq!(refreshed_messages[0].client, "cc-mirror/kimi-code");
            assert_eq!(refreshed_messages[0].provider_id, "minimax");
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_all_messages_keeps_normal_claude_when_cc_mirror_points_at_claude_config() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);

        {
            let claude_dir = source_home.path().join(".claude");
            let project_dir = claude_dir.join("projects/project-one");
            std::fs::create_dir_all(&project_dir).unwrap();
            let session_path = project_dir.join("session.jsonl");
            std::fs::write(
                &session_path,
                r#"{"type":"assistant","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_001","message":{"id":"msg_001","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}
"#,
            )
            .unwrap();

            let variant_dir = source_home.path().join(".cc-mirror/plain-mirror");
            std::fs::create_dir_all(&variant_dir).unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                serde_json::json!({
                    "name": "plain-mirror",
                    "provider": "mirror",
                    "configDir": claude_dir,
                })
                .to_string(),
            )
            .unwrap();

            let messages = parse_all_messages_with_pricing(
                source_home.path().to_str().unwrap(),
                &["claude".to_string()],
                None,
            );
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].client, "claude");
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_parse_local_clients_amp_partial_ledger_recovers_message_fallback_day() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        use chrono::TimeZone;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let amp_dir = temp_dir.path().join(".local/share/amp/threads");
        std::fs::create_dir_all(&amp_dir).unwrap();

        let thread_created = chrono::DateTime::parse_from_rfc3339("2026-04-04T12:00:00Z")
            .unwrap()
            .timestamp_millis();
        let ledger_timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-08T12:00:00Z")
            .unwrap()
            .timestamp_millis();

        let thread = format!(
            r#"{{
                "id": "thread-amp-gap",
                "created": {thread_created},
                "usageLedger": {{
                    "events": [
                        {{
                            "timestamp": "2026-04-08T12:00:00Z",
                            "model": "claude-sonnet-4-0",
                            "credits": 0.75,
                            "tokens": {{ "input": 100, "output": 20 }}
                        }}
                    ]
                }},
                "messages": [
                    {{
                        "role": "assistant",
                        "messageId": 1,
                        "usage": {{
                            "model": "claude-sonnet-4-0",
                            "inputTokens": 100,
                            "outputTokens": 20,
                            "credits": 0.75
                        }}
                    }},
                    {{
                        "role": "assistant",
                        "messageId": 2,
                        "usage": {{
                            "model": "claude-sonnet-4-0",
                            "inputTokens": 50,
                            "outputTokens": 10,
                            "credits": 0.40
                        }}
                    }}
                ]
            }}"#
        );
        std::fs::write(amp_dir.join("T-thread-amp-gap.json"), thread).unwrap();

        let parsed = parse_local_clients(LocalParseOptions {
            home_dir: Some(temp_dir.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["amp".to_string()]),
            since: None,
            until: None,
            year: None,
            scanner_settings: scanner::ScannerSettings::default(),
            modified_after: None,
        })
        .unwrap();

        assert_eq!(parsed.counts.get(ClientId::Amp), 2);
        assert_eq!(parsed.messages.len(), 2);

        let dates: HashSet<String> = parsed.messages.iter().map(|msg| msg.date.clone()).collect();
        let local_date = |timestamp_ms: i64| {
            chrono::Local
                .timestamp_millis_opt(timestamp_ms)
                .single()
                .unwrap()
                .format("%Y-%m-%d")
                .to_string()
        };
        assert!(dates.contains(&local_date(thread_created + 2000)));
        assert!(dates.contains(&local_date(ledger_timestamp)));
    }

    // =========================================================================
    // fold_messages_streaming parity tests (RED — fold_messages_streaming not yet impl)
    // =========================================================================

    /// Deterministic UnifiedMessage fixture helper shared with parity tests.
    /// Uses no real JSONL files; all fields are constructed inline.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn parity_msg(
        date: &str,
        client: &str,
        model: &str,
        session_id: &str,
        dedup_key: Option<&str>,
        timestamp_ms: i64,
        input: i64,
        output: i64,
        cost: f64,
    ) -> crate::sessions::UnifiedMessage {
        use crate::TokenBreakdown;
        crate::sessions::UnifiedMessage {
            client: client.to_string(),
            model_id: model.to_string(),
            provider_id: "anthropic".to_string(),
            session_id: session_id.to_string(),
            workspace_key: None,
            workspace_label: None,
            timestamp: timestamp_ms,
            date: date.to_string(),
            tokens: TokenBreakdown {
                input,
                output,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
                cache_write_1h: 0,
            },
            cost,
            cost_source: crate::CostSource::Unknown,
            duration_ms: None,
            message_count: 1,
            agent: None,
            dedup_key: dedup_key.map(|s| s.to_string()),
            dedup_aliases: Vec::new(),
            is_turn_start: false,
            input_tokens_are_cache_split: false,
        }
    }

    // A/B parity: fold_messages_streaming output == aggregate_by_date output
    // for the same deterministic fixture (no dedup_keys, no trae).
    #[test]
    fn test_fold_messages_streaming_parity_with_aggregate_by_date_no_dedup() {
        let messages = vec![
            parity_msg(
                "2025-06-01",
                "claude",
                "claude-sonnet-4-5",
                "s1",
                None,
                1_748_000_000_000,
                100,
                50,
                0.01,
            ),
            parity_msg(
                "2025-06-01",
                "opencode",
                "gpt-4o",
                "s2",
                None,
                1_748_000_001_000,
                200,
                100,
                0.02,
            ),
            parity_msg(
                "2025-06-02",
                "codex",
                "gpt-5",
                "s3",
                None,
                1_748_086_400_000,
                400,
                200,
                0.04,
            ),
        ];

        // Reference: existing aggregate_by_date (clone-based)
        let reference = crate::aggregator::aggregate_by_date(messages.clone());

        // Subject: new streaming path
        let streaming = fold_messages_streaming(&messages);

        assert_eq!(
            reference.len(),
            streaming.len(),
            "parity: day bucket count must match"
        );
        for (ref_day, stream_day) in reference.iter().zip(streaming.iter()) {
            assert_eq!(ref_day.date, stream_day.date, "parity: date must match");
            assert_eq!(
                ref_day.totals.tokens, stream_day.totals.tokens,
                "parity: tokens must match for date {}",
                ref_day.date
            );
            assert!(
                (ref_day.totals.cost - stream_day.totals.cost).abs() < 1e-9,
                "parity: cost must match for date {}",
                ref_day.date
            );
            assert_eq!(
                ref_day.totals.messages, stream_day.totals.messages,
                "parity: message_count must match for date {}",
                ref_day.date
            );
        }
    }

    // A/B parity with cross-file dedup: fold_messages_streaming must apply
    // the same dedup_key filtering as the existing pipeline does via seen_keys.
    #[test]
    fn test_fold_messages_streaming_parity_cross_file_dedup() {
        // Construct messages that include a duplicated dedup_key pair.
        // The existing pipeline filters duplicates via the seen_keys HashSet.
        // fold_messages_streaming must produce the same counts.
        let unique = parity_msg(
            "2025-06-10",
            "claude",
            "claude-sonnet-4-5",
            "u1",
            Some("unique-key-1"),
            1_749_000_000_000,
            300,
            150,
            0.06,
        );
        let dup_first = parity_msg(
            "2025-06-10",
            "claude",
            "claude-sonnet-4-5",
            "d1",
            Some("dup-key-shared"),
            1_749_000_001_000,
            200,
            100,
            0.04,
        );
        let dup_second = parity_msg(
            "2025-06-10",
            "claude",
            "claude-haiku-4-5",
            "d2",
            Some("dup-key-shared"),
            1_749_000_002_000,
            200,
            100,
            0.04,
        );

        // The reference pipeline keeps only the first occurrence of dup-key-shared
        // (seen_keys.insert returns false on second) — 2 messages total.
        let all_msgs = vec![unique.clone(), dup_first.clone(), dup_second.clone()];

        let streaming = fold_messages_streaming(&all_msgs);

        assert_eq!(streaming.len(), 1, "all on same date -> 1 bucket");
        assert_eq!(
            streaming[0].totals.messages, 2,
            "parity: duplicate dedup_key must reduce count from 3 to 2"
        );
        assert!(
            (streaming[0].totals.cost - 0.10).abs() < 1e-9,
            "parity: cost must exclude the duplicate message"
        );
    }

    // =========================================================================
    // Phase 2 RED tests: build_graph_result_from_messages streaming entry-point
    // =========================================================================
    //
    // `build_graph_result_from_messages` does NOT exist yet.  These tests
    // define the observable contract that the GREEN implementation must satisfy:
    //   - accept a `&[UnifiedMessage]` slice and an optional `since` date string
    //   - apply the `since` post-parse filter (date string prefix comparison,
    //     same semantics as `filter_messages_for_report`)
    //   - drive aggregation via `StreamingAggregator` (zero-clone fold path)
    //   - return a `GraphResult` whose per-day tokens/cost match the reference
    //     `aggregate_by_date` pipeline exactly (zero tolerance)
    //
    // All three tests will produce a **compile error** until the function is
    // declared in lib.rs, which is the required RED state.

    use crate::GraphResult;

    /// Multi-client, multi-day fixture: streaming path total tokens and cost
    /// per daily bucket must match hand-computed expected values.
    ///
    /// Hardcoded expected values — calculation:
    ///
    /// 2025-06-01 (no dedup):
    ///   s1: input=500, output=250 → tokens=750, cost=0.05
    ///   s2: input=300, output=150 → tokens=450, cost=0.03
    ///   TOTAL: tokens=1200, cost=0.08, messages=2
    ///
    /// 2025-06-02 (trae session dedup — same session_id="trae-sess"):
    ///   trae-k1: ts=1_748_822_400_000, input=100, output=50  → tokens=150, cost=0.01
    ///   trae-k2: ts=1_748_822_500_000, input=200, output=100 → tokens=300, cost=0.02
    ///   StreamingAggregator keeps latest timestamp → trae-k2 wins
    ///   TOTAL: tokens=300, cost=0.02, messages=1
    ///
    /// 2025-06-03 (cross-file dedup_key — both carry "dup-phase2"):
    ///   d1: input=400, output=200 → tokens=600, cost=0.04  (first seen — kept)
    ///   d2: input=400, output=200 → tokens=600, cost=0.04  (same dedup_key — dropped)
    ///   TOTAL: tokens=600, cost=0.04, messages=1
    #[test]
    fn test_build_graph_result_from_messages_matches_aggregate_by_date() {
        let messages = vec![
            // Day 2025-06-01: two clients, no dedup
            parity_msg(
                "2025-06-01",
                "claude",
                "claude-sonnet-4-5",
                "s1",
                None,
                1_748_736_000_000,
                500,
                250,
                0.05,
            ),
            parity_msg(
                "2025-06-01",
                "opencode",
                "gpt-4o",
                "s2",
                None,
                1_748_736_001_000,
                300,
                150,
                0.03,
            ),
            // Day 2025-06-02: trae dedup by session_id — two entries same session, keep latest
            parity_msg(
                "2025-06-02",
                "trae",
                "gpt-5.2",
                "trae-sess",
                Some("trae-k1"),
                1_748_822_400_000,
                100,
                50,
                0.01,
            ),
            parity_msg(
                "2025-06-02",
                "trae",
                "gpt-5.2",
                "trae-sess",
                Some("trae-k2"),
                1_748_822_500_000,
                200,
                100,
                0.02,
            ), // newer timestamp -> wins
            // Day 2025-06-03: cross-file dedup pair — same dedup_key, second dropped
            parity_msg(
                "2025-06-03",
                "claude",
                "claude-haiku-4-5",
                "d1",
                Some("dup-phase2"),
                1_748_908_800_000,
                400,
                200,
                0.04,
            ),
            parity_msg(
                "2025-06-03",
                "claude",
                "claude-haiku-4-5",
                "d2",
                Some("dup-phase2"),
                1_748_908_801_000,
                400,
                200,
                0.04,
            ), // same dedup_key -> discarded
        ];

        // Subject: new streaming entry-point
        let result: GraphResult = crate::build_graph_result_from_messages(&messages, None);

        // Verify bucket count: 3 distinct dates
        assert_eq!(
            result.contributions.len(),
            3,
            "phase2 streaming: must produce exactly 3 daily buckets"
        );

        // Locate each day bucket by date (sort order: ascending)
        let day1 = result
            .contributions
            .iter()
            .find(|c| c.date == "2025-06-01")
            .expect("phase2: 2025-06-01 bucket must exist");
        let day2 = result
            .contributions
            .iter()
            .find(|c| c.date == "2025-06-02")
            .expect("phase2: 2025-06-02 bucket must exist");
        let day3 = result
            .contributions
            .iter()
            .find(|c| c.date == "2025-06-03")
            .expect("phase2: 2025-06-03 bucket must exist");

        // 2025-06-01: s1 (750) + s2 (450) = 1200 tokens, 0.05+0.03=0.08 cost, 2 messages
        assert_eq!(
            day1.totals.tokens, 1200,
            "2025-06-01: tokens must be 750+450=1200"
        );
        assert!(
            (day1.totals.cost - 0.08).abs() < 1e-9,
            "2025-06-01: cost must be 0.05+0.03=0.08"
        );
        assert_eq!(
            day1.totals.messages, 2,
            "2025-06-01: both non-trae non-dedup messages must be counted"
        );

        // 2025-06-02: trae session dedup — trae-k2 wins (larger timestamp)
        // trae-k2: input=200, output=100 -> tokens=300, cost=0.02
        assert_eq!(
            day2.totals.tokens, 300,
            "2025-06-02: trae dedup — only winner (trae-k2, tokens=300) counted"
        );
        assert!(
            (day2.totals.cost - 0.02).abs() < 1e-9,
            "2025-06-02: trae dedup — cost must be 0.02 (trae-k2 only)"
        );
        assert_eq!(
            day2.totals.messages, 1,
            "2025-06-02: trae dedup collapses 2 entries to 1 per session_id"
        );

        // 2025-06-03: cross-file dedup — d1 kept, d2 dropped (same dedup_key)
        // d1: input=400, output=200 -> tokens=600, cost=0.04
        assert_eq!(
            day3.totals.tokens, 600,
            "2025-06-03: cross-file dedup — only d1 (tokens=600) counted, d2 dropped"
        );
        assert!(
            (day3.totals.cost - 0.04).abs() < 1e-9,
            "2025-06-03: cross-file dedup — cost must be 0.04 (d1 only)"
        );
        assert_eq!(
            day3.totals.messages, 1,
            "2025-06-03: duplicate dedup_key dropped, 1 message retained"
        );
    }

    /// `since` filter semantics: same fixture with `since = "2025-06-02"` must
    /// produce only the 2025-06-02 and 2025-06-03 buckets, with their
    /// token/cost totals matching a manually filtered reference.
    #[test]
    fn test_build_graph_result_from_messages_since_filter_excludes_earlier_dates() {
        let messages = vec![
            parity_msg(
                "2025-06-01",
                "claude",
                "claude-sonnet-4-5",
                "s1",
                None,
                1_748_736_000_000,
                500,
                250,
                0.05,
            ),
            parity_msg(
                "2025-06-01",
                "opencode",
                "gpt-4o",
                "s2",
                None,
                1_748_736_001_000,
                300,
                150,
                0.03,
            ),
            parity_msg(
                "2025-06-02",
                "codex",
                "gpt-5",
                "s3",
                None,
                1_748_822_400_000,
                400,
                200,
                0.04,
            ),
            parity_msg(
                "2025-06-03",
                "claude",
                "claude-haiku-4-5",
                "s4",
                None,
                1_748_908_800_000,
                200,
                100,
                0.02,
            ),
        ];

        // Subject: streaming entry with since = "2025-06-02"
        // (function does not exist yet -> RED compile error)
        let result: GraphResult =
            crate::build_graph_result_from_messages(&messages, Some("2025-06-02"));

        // Only 2025-06-02 and 2025-06-03 must be present
        assert_eq!(
            result.contributions.len(),
            2,
            "since filter: must exclude 2025-06-01, leaving 2 buckets"
        );

        let dates: Vec<&str> = result
            .contributions
            .iter()
            .map(|c| c.date.as_str())
            .collect();
        assert!(
            dates.contains(&"2025-06-02"),
            "since filter: 2025-06-02 bucket must be present"
        );
        assert!(
            dates.contains(&"2025-06-03"),
            "since filter: 2025-06-03 bucket must be present"
        );
        assert!(
            !dates.contains(&"2025-06-01"),
            "since filter: 2025-06-01 bucket must be absent"
        );

        // 2025-06-02 token total: input 400 + output 200 = 600
        let day2 = result
            .contributions
            .iter()
            .find(|c| c.date == "2025-06-02")
            .unwrap();
        assert_eq!(
            day2.totals.tokens, 600,
            "since filter: 2025-06-02 token total must be 600"
        );
        assert!(
            (day2.totals.cost - 0.04).abs() < 1e-9,
            "since filter: 2025-06-02 cost must be 0.04"
        );
    }

    /// Trae dedup in streaming path: two messages for the same trae session
    /// (same `session_id`, different `dedup_key`, later timestamp wins) must
    /// produce exactly ONE message worth of tokens/cost in the daily bucket.
    #[test]
    fn test_build_graph_result_from_messages_trae_session_dedup_keeps_latest() {
        let messages = vec![
            // Earlier trae message (should be dropped)
            parity_msg(
                "2025-06-10",
                "trae",
                "gpt-5.2",
                "trae-sess-a",
                Some("trae-early"),
                1_749_513_600_000,
                100,
                50,
                0.01,
            ),
            // Later trae message for same session_id (should win)
            parity_msg(
                "2025-06-10",
                "trae",
                "gpt-5.2",
                "trae-sess-a",
                Some("trae-late"),
                1_749_513_700_000,
                300,
                150,
                0.03,
            ),
            // Non-trae message (should be included as-is)
            parity_msg(
                "2025-06-10",
                "claude",
                "claude-sonnet-4-5",
                "c1",
                None,
                1_749_513_800_000,
                200,
                100,
                0.02,
            ),
        ];

        // Subject: streaming entry (does not exist yet -> RED compile error)
        let result: GraphResult = crate::build_graph_result_from_messages(&messages, None);

        assert_eq!(
            result.contributions.len(),
            1,
            "trae dedup: all messages on same date -> 1 bucket"
        );

        let day = &result.contributions[0];
        // Kept messages: trae-late (tokens=450) + claude (tokens=300) = 750 total tokens
        assert_eq!(
            day.totals.tokens, 750,
            "trae dedup: token total must reflect only the winning trae entry (450) + claude (300)"
        );
        assert!(
            (day.totals.cost - 0.05).abs() < 1e-9,
            "trae dedup: cost must be 0.03 (latest trae) + 0.02 (claude) = 0.05"
        );
        assert_eq!(
            day.totals.messages, 2,
            "trae dedup: message count must be 2 (1 trae winner + 1 claude)"
        );
    }

    #[test]
    fn model_aggregation_saturates_overflowing_token_folds() {
        // token_total_saturates_on_overlarge_buckets (see positive_token_total's
        // callers) covers a single message's grand total; the per-field
        // CROSS-MESSAGE fold in aggregate_model_usage_entries must saturate too.
        // An antigravity-cli row can carry an i64::MAX bucket after the
        // untrusted-varint clamp (sessions/antigravity_cli.rs to_i64), so two
        // such rows folded into one model group with plain `+=` overflow (debug
        // panic / release wrap) before the already-saturating grand total runs.
        let make = || {
            UnifiedMessage::new(
                "antigravity-cli",
                "gemini-3-pro",
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
            )
        };

        let entries = aggregate_model_usage_entries(vec![make(), make()], &GroupBy::Model);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, i64::MAX);
        assert_eq!(entries[0].cache_read, i64::MAX);
    }

    #[test]
    fn model_report_totals_saturate_across_groups() {
        // aggregate_model_usage_entries saturates each entry's fields, so an
        // entry can be i64::MAX. get_model_report sums the entries into the
        // report-level totals via model_report_token_totals; two saturated
        // entries (two distinct models) must not overflow that sum either.
        let make = |model: &str| {
            UnifiedMessage::new(
                "antigravity-cli",
                model,
                "antigravity",
                "session-overflow",
                1_733_011_200_000,
                TokenBreakdown {
                    input: i64::MAX,
                    output: 0,
                    cache_read: i64::MAX,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
            )
        };

        let entries = aggregate_model_usage_entries(
            vec![make("gemini-3-pro"), make("claude-opus-4-6")],
            &GroupBy::Model,
        );
        assert_eq!(entries.len(), 2);
        let (total_input, _total_output, total_cache_read, _total_cache_write) =
            super::model_report_token_totals(&entries);
        assert_eq!(total_input, i64::MAX);
        assert_eq!(total_cache_read, i64::MAX);
    }

    #[cfg(target_os = "macos")]
    fn m15a_global_root(home: &Path) -> PathBuf {
        home.join("Library/Application Support/Kiro/User/globalStorage/kiro.kiroagent")
    }

    #[cfg(target_os = "macos")]
    fn write_m15a_snapshot(home: &Path, body: &str) -> PathBuf {
        let path = m15a_global_root(home).join("workspace-a/conversation.chat");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        m15a_global_root(home).join("workspace-a/conversation.chat")
    }

    #[cfg(target_os = "macos")]
    fn write_m15a_execution(home: &Path, status: &str, start_time: &str) -> PathBuf {
        let path = m15a_global_root(home).join("workspace-a/execution-store/execution");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{
                    "executionId": "exec-1",
                    "chatSessionId": "chat-1",
                    "status": "{status}",
                    "startTime": {start_time},
                    "endTime": 1770983427500,
                    "completionOptions": {{"modelId": "claude-sonnet-4-5"}},
                    "context": {{"messages": [{{"entries": [{{"type": "text", "text": "execution input"}}]}}]}},
                    "actions": [{{"actionType": "say", "output": "execution output"}}]
                }}"#
            ),
        )
        .unwrap();
        path
    }

    #[cfg(target_os = "macos")]
    fn m15a_snapshot_body(execution_id: &str, prompt: &str, response: &str) -> String {
        format!(
            r#"{{
                "executionId": "{execution_id}",
                "model": "claude-sonnet-4-5",
                "messages": [
                    {{"role": "user", "content": "{prompt}"}},
                    {{"role": "assistant", "content": "{response}"}}
                ]
            }}"#
        )
    }

    fn m15a_local_options(home: &Path) -> LocalParseOptions {
        LocalParseOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["kiro".to_string()]),
            ..Default::default()
        }
    }

    // Upstream #1342, credits half: Kiro credits become a provider-reported
    // cost that token pricing must not replace on the materialized and
    // streaming lanes, from both the CLI files and the SQLite database, while
    // an uncredited turn of the same conversation is still token-priced. The count lane carries no cost; it reports summed
    // request counts. A v1 cache entry of the unchanged CLI file (cost 0,
    // request count 1) must be re-parsed, not replayed.
    #[test]
    #[serial_test::serial]
    fn kiro_credit_cost_survives_pricing_and_replaces_a_v1_entry() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let cli_dir = source_home.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("credited.json"),
            r#"{"session_id":"credited","cwd":"/tmp/project","session_state":{"rts_model_state":{"model_info":{"model_id":"claude-sonnet-4-5","context_window_tokens":1000}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":0,"output_token_count":0,"total_request_count":3,"message_ids":["a-1"],"context_usage_percentage":10.0,"metering_usage":[{"value":0.25,"unit":"credit"}]},{"input_token_count":0,"output_token_count":0,"message_ids":["a-2"]}]}}}"#,
        )
        .unwrap();
        std::fs::write(
            cli_dir.join("credited.jsonl"),
            concat!(
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"p-1","content":[{"kind":"text","data":"hello"}],"meta":{"timestamp":1770983426.0}}}"#,
                "\n",
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"a-1","content":[{"kind":"text","data":"response text"}]}}"#,
                "\n",
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"p-2","content":[{"kind":"text","data":"more"}],"meta":{"timestamp":1770983500.0}}}"#,
                "\n",
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"a-2","content":[{"kind":"text","data":"done"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let db_dir = source_home.path().join(".local/share/kiro-cli");
        std::fs::create_dir_all(&db_dir).unwrap();
        let connection = rusqlite::Connection::open(db_dir.join("data.sqlite3")).unwrap();
        connection
            .execute(
                "CREATE TABLE conversations_v2 (key TEXT, conversation_id TEXT, value TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO conversations_v2 (key, conversation_id, value) VALUES (?1, ?2, ?3)",
                (
                    "/tmp/project",
                    "conv-1",
                    r#"{"model_info":{"model_id":"claude-sonnet-4-5","context_window_tokens":1000},"user_turn_metadata":{"usage_info":[{"value":0.5,"unit":"credit"}]},"history":[{"request_metadata":{"context_usage_percentage":10,"response_size":40,"request_start_timestamp_ms":1770983426000,"request_count":2}},{"request_metadata":{"response_size":8,"request_start_timestamp_ms":1770983428000}}]}"#,
                ),
            )
            .unwrap();
        drop(connection);
        let mut pricing_data = HashMap::new();
        pricing_data.insert(
            "claude-sonnet-4-5".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(1.0),
                output_cost_per_token: Some(1.0),
                ..Default::default()
            },
        );
        let pricing_service = pricing::PricingService::new(pricing_data, HashMap::new());
        let home = source_home.path().to_str().unwrap();
        let clients = ["kiro".to_string()];
        let materialized = || {
            parse_all_messages_with_pricing_with_env_strategy(
                home,
                &clients,
                Some(&pricing_service),
                false,
                &scanner::ScannerSettings::default(),
                None,
            )
        };
        let streamed = || {
            let mut streamed = Vec::new();
            scan_messages_streaming(
                home,
                &clients,
                Some(&pricing_service),
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |message| streamed.push(message.clone()),
            );
            streamed
        };
        let assert_credit = |mut messages: Vec<UnifiedMessage>, lane: &str| {
            messages.sort_by(|a, b| a.dedup_key.cmp(&b.dedup_key));
            let view: Vec<_> = messages
                .iter()
                .map(|m| {
                    (
                        m.dedup_key.clone().unwrap(),
                        m.cost,
                        m.has_authoritative_cost(),
                        m.message_count,
                        m.tokens.input,
                        m.tokens.output,
                    )
                })
                .collect();
            // Token pricing gives (input + output) * 1.0; a credited message
            // must keep its credit cost instead.
            assert_eq!(
                view,
                vec![
                    ("conv-1:0".to_string(), 0.5 * 0.04, true, 2, 100, 10),
                    ("conv-1:1".to_string(), 2.0, false, 1, 0, 2),
                    ("credited:0".to_string(), 0.25 * 0.04, true, 3, 100, 4),
                    ("credited:1".to_string(), 2.0, false, 1, 1, 1),
                ],
                "{lane}"
            );
        };

        assert_credit(materialized(), "materialized, cold");
        assert_credit(streamed(), "streaming, warm");
        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 3 + 1 + 2 + 1);

        // What a v1 build cached for the same, unchanged CLI file.
        let plant_v1_entry = || {
            let identity = message_cache::CacheIdentity::for_client(ClientId::Kiro);
            let mut cache = message_cache::SourceMessageCache::load();
            let mut entry = cache
                .entries_for_test()
                .into_iter()
                .find(|entry| {
                    entry
                        .path
                        .to_path_buf()
                        .file_name()
                        .is_some_and(|name| name == "credited.json")
                })
                .expect("the CLI source must be cached")
                .clone();
            for message in entry.messages.iter_mut() {
                message.cost = 0.0;
                message.cost_source = sessions::CostSource::Unknown;
                message.message_count = 1;
            }
            cache.insert(entry);
            cache.save_if_dirty();
            message_cache::set_parser_version_for_test(identity, 1);
        };

        plant_v1_entry();
        assert_credit(streamed(), "streaming over a v1 entry");
        plant_v1_entry();
        assert_credit(materialized(), "materialized over a v1 entry");
    }

    fn m15a_report_options(home: &Path) -> ReportOptions {
        ReportOptions {
            home_dir: Some(home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(vec!["kiro".to_string()]),
            ..Default::default()
        }
    }

    fn m15b_ide_paths(home: &Path) -> (PathBuf, PathBuf) {
        let dir = home.join(".kiro/sessions/workspace-a/sess_m15b");
        (dir.join("session.json"), dir.join("messages.jsonl"))
    }

    #[test]
    #[serial_test::serial]
    fn m15b_ide_sibling_changes_reach_cache_all_lanes_and_reports() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let (session, messages) = m15b_ide_paths(source_home.path());
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::write(
            &session,
            r#"{
                "id":"sess_m15b",
                "modelId":"claude-opus-4.6",
                "workspacePaths":["/tmp/m15b-project"]
            }"#,
        )
        .unwrap();

        let clients = ["kiro".to_string()];
        let scanner_settings = scanner::ScannerSettings::default();
        let scan_result = scan_test_sources(source_home.path(), &clients, &scanner_settings);
        let cache_session = scanner_spelling(scan_result.get(ClientId::Kiro), &session);
        let parse_materialized = || {
            let mut parsed = parse_all_messages_with_pricing_with_env_strategy(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner_settings,
                None,
            );
            parsed.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
            parsed
        };

        let source_fingerprint = message_cache::SourceFingerprint::from_path(&session).unwrap();
        let missing_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        let before = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(parse_materialized().is_empty());

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &messages,
            concat!(
                "{\"timestamp\":\"2026-06-20T10:00:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"hello\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":10.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"answer\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:00:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
            ),
        )
        .unwrap();
        let first_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        assert_ne!(missing_sidecar_fingerprint, first_sidecar_fingerprint);
        assert_eq!(
            source_fingerprint,
            message_cache::SourceFingerprint::from_path(&session).unwrap(),
            "messages.jsonl must invalidate through the related-file fingerprint"
        );
        let after_first = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(after_first > before);

        let first = parse_materialized();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].tokens.input, 20_000);
        assert_eq!(first[0].dedup_key.as_deref(), Some("sess_m15b:ide:0"));
        assert_eq!(first[0].model_id, "claude-opus-4.6");
        assert_eq!(first[0].workspace_key.as_deref(), Some("/tmp/m15b-project"));
        assert_eq!(
            parse_materialized(),
            first,
            "the first warm hit must be stable"
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &messages,
            concat!(
                "{\"timestamp\":\"2026-06-20T10:00:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"hello\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":10.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"answer\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:00:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
                "{\"timestamp\":\"2026-06-20T10:01:00Z\",\"payload\":{\"type\":\"user\",\"content\":\"next\"}}\n",
                "{\"payload\":{\"type\":\"session_metadata\",\"key\":\"contextUsage\",\"value\":{\"usagePercentage\":20.0}}}\n",
                "{\"payload\":{\"type\":\"assistant\",\"content\":\"done\"}}\n",
                "{\"payload\":{\"type\":\"usage_summary\",\"elapsedTime\":1000}}\n",
                "{\"timestamp\":\"2026-06-20T10:01:01Z\",\"payload\":{\"type\":\"turn_end\"}}\n",
            ),
        )
        .unwrap();
        let updated_sidecar_fingerprint =
            message_cache::SourceFingerprint::from_kiro_path(&session).unwrap();
        assert_ne!(first_sidecar_fingerprint, updated_sidecar_fingerprint);
        let after_second = latest_source_mtime_ms(&m15a_local_options(source_home.path())).unwrap();
        assert!(after_second > after_first);

        let updated = parse_materialized();
        assert_eq!(updated.len(), 2);
        assert_eq!(
            updated
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            60_000
        );
        assert_eq!(
            parse_materialized(),
            updated,
            "the rebuilt cache must stay warm-complete"
        );
        assert_eq!(
            message_cache::SourceMessageCache::load()
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Kiro),
                    &cache_session,
                )
                .unwrap()
                .messages
                .len(),
            2
        );

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &clients,
            None,
            false,
            &scanner_settings,
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        streamed.sort_by(|left, right| left.dedup_key.cmp(&right.dedup_key));
        assert_eq!(streamed, updated);

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 2);
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(
            counted
                .messages
                .iter()
                .map(|message| message.input)
                .sum::<i64>(),
            60_000
        );
        let pruned = parse_local_clients(LocalParseOptions {
            modified_after: Some(after_first + 1),
            ..m15a_local_options(source_home.path())
        })
        .unwrap();
        assert_eq!(pruned.counts.get(ClientId::Kiro), 2);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report_options = m15a_report_options(source_home.path());
        let model = runtime
            .block_on(get_model_report(report_options.clone()))
            .unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(report_options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(report_options.clone()))
            .unwrap();
        let agents = runtime.block_on(get_agents_report(report_options)).unwrap();
        assert_eq!(model.total_messages, 2);
        assert_eq!(model.total_input, 60_000);
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            2
        );
        assert_eq!(agents.total_messages, 2);
        assert_eq!(
            agents.entries.iter().map(|entry| entry.input).sum::<i64>(),
            60_000
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_cli_keys_cannot_seed_or_collide_with_globalstorage() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let cli_dir = source_home.path().join(".kiro/sessions/cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("cli.json"),
            r#"{"session_id":"execution","cwd":"workspace-a","session_state":{"rts_model_state":{"model_info":{"model_id":"cli-model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":1}]}}}"#,
        )
        .unwrap();
        std::fs::write(cli_dir.join("cli.jsonl"), "").unwrap();
        std::fs::write(
            cli_dir.join("cli-collision.json"),
            r#"{"session_id":"workspace-a/conversation:globalstorage:exec","cwd":"workspace-a","session_state":{"rts_model_state":{"model_info":{"model_id":"cli-model"}},"conversation_metadata":{"user_turn_metadatas":[{"input_token_count":1}]}}}"#,
        )
        .unwrap();
        std::fs::write(cli_dir.join("cli-collision.jsonl"), "").unwrap();
        write_m15a_snapshot(source_home.path(), &m15a_snapshot_body("0", "ABCD", ""));
        let execution_zero =
            m15a_global_root(source_home.path()).join("workspace-a/execution-store/execution-zero");
        std::fs::create_dir_all(execution_zero.parent().unwrap()).unwrap();
        std::fs::write(
            execution_zero,
            r#"{
                "executionId": "0",
                "chatSessionId": "conversation",
                "status": "succeed",
                "startTime": 1770983426,
                "endTime": 1770983427500,
                "completionOptions": {"modelId": "claude-sonnet-4-5"},
                "context": {"messages": [{"entries": [{"type": "text", "text": "execution input"}]}]},
                "actions": [{"actionType": "say", "output": "execution output"}]
            }"#,
        )
        .unwrap();

        let expected = vec![
            "conversation",
            "execution",
            "workspace-a/conversation:globalstorage:exec",
        ];
        let mut materialized: Vec<_> = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        )
        .into_iter()
        .map(|message| message.session_id)
        .collect();
        materialized.sort();
        assert_eq!(materialized, expected);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.session_id.clone()),
        );
        streamed.sort();
        assert_eq!(streamed, expected);

        let mut counted: Vec<_> = parse_local_clients(m15a_local_options(source_home.path()))
            .unwrap()
            .messages
            .into_iter()
            .map(|message| message.session_id)
            .collect();
        counted.sort();
        assert_eq!(counted, expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_duplicate_snapshot_extensions_are_exact_once() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let body = m15a_snapshot_body("unused", "ABCD", "WXYZ");
        let chat = write_m15a_snapshot(source_home.path(), &body);
        std::fs::write(chat.with_extension("json"), body).unwrap();

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(materialized.len(), 1);

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 1);
        assert_eq!(counted.messages.len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_materialized_streaming_count_and_report_parity() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "snapshot input", "snapshot output"),
        );
        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let mut pricing_data = HashMap::new();
        pricing_data.insert(
            "claude-sonnet-4-5".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(1.0),
                output_cost_per_token: Some(1.0),
                ..Default::default()
            },
        );
        let pricing_service = pricing::PricingService::new(pricing_data, HashMap::new());

        let materialized = parse_all_messages_with_pricing_with_env_strategy(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            Some(&pricing_service),
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(materialized.len(), 1);
        assert_eq!(
            materialized[0].dedup_key.as_deref(),
            Some("execution:exec-1")
        );
        assert!(materialized[0].cost > 0.0);

        // Both source entries are raw and independently cached even though the
        // snapshot is suppressed in the merged result.
        let cache = message_cache::SourceMessageCache::load();
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Kiro),
                &snapshot
            )
            .is_some_and(|entry| entry.messages[0]
                .dedup_key
                .as_deref()
                .is_some_and(|key| key.ends_with(":globalstorage:exec:exec-1"))));
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Kiro),
                &execution
            )
            .is_some_and(
                |entry| entry.messages[0].dedup_key.as_deref() == Some("execution:exec-1")
            ));
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Kiro),
                &snapshot,
            )
            .is_some_and(|entry| entry.messages[0].cost == 0.0));
        assert!(cache
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Kiro),
                &execution,
            )
            .is_some_and(|entry| entry.messages[0].cost == 0.0));

        let mut streamed = Vec::new();
        scan_messages_streaming(
            source_home.path().to_str().unwrap(),
            &["kiro".to_string()],
            Some(&pricing_service),
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].dedup_key.as_deref(), Some("execution:exec-1"));
        assert!(streamed[0].cost > 0.0);
        assert_eq!(materialized[0].cost, streamed[0].cost);
        assert_eq!(
            (materialized[0].tokens.input, materialized[0].tokens.output),
            (streamed[0].tokens.input, streamed[0].tokens.output)
        );
        assert_eq!(materialized[0].model_id, "claude-sonnet-4-5");
        assert_eq!(materialized[0].session_id, "chat-1");
        assert_eq!(
            materialized[0].workspace_key.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            materialized[0].workspace_label.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(materialized[0].timestamp, 1_770_983_426_000);
        assert_eq!(materialized[0].duration_ms, Some(1_500));
        assert_eq!(materialized[0].message_count, 1);
        assert_eq!(
            (
                streamed[0].model_id.as_str(),
                streamed[0].session_id.as_str(),
                streamed[0].workspace_key.as_deref(),
                streamed[0].workspace_label.as_deref(),
                streamed[0].timestamp,
                streamed[0].duration_ms,
                streamed[0].message_count,
            ),
            (
                materialized[0].model_id.as_str(),
                materialized[0].session_id.as_str(),
                materialized[0].workspace_key.as_deref(),
                materialized[0].workspace_label.as_deref(),
                materialized[0].timestamp,
                materialized[0].duration_ms,
                materialized[0].message_count,
            )
        );

        let counted = parse_local_clients(m15a_local_options(source_home.path())).unwrap();
        assert_eq!(counted.counts.get(ClientId::Kiro), 1);
        assert_eq!(counted.messages.len(), 1);
        assert_eq!(counted.messages[0].model_id, materialized[0].model_id);
        assert_eq!(counted.messages[0].session_id, materialized[0].session_id);
        assert_eq!(
            counted.messages[0].workspace_key,
            materialized[0].workspace_key
        );
        assert_eq!(
            counted.messages[0].workspace_label,
            materialized[0].workspace_label
        );
        assert_eq!(counted.messages[0].timestamp, materialized[0].timestamp);
        assert_eq!(counted.messages[0].duration_ms, materialized[0].duration_ms);
        assert_eq!(
            counted.messages[0].message_count,
            materialized[0].message_count
        );

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = m15a_report_options(source_home.path());
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        let agents = runtime
            .block_on(get_agents_report(options.clone()))
            .unwrap();
        let mut session_options = options.clone();
        session_options.group_by = GroupBy::Session;
        let session_model = runtime.block_on(get_model_report(session_options)).unwrap();
        let mut workspace_options = options;
        workspace_options.group_by = GroupBy::WorkspaceModel;
        let workspace_model = runtime
            .block_on(get_model_report(workspace_options))
            .unwrap();

        assert_eq!(model.total_messages, 1);
        assert_eq!(model.entries.len(), 1);
        assert_eq!(model.entries[0].model, materialized[0].model_id);
        assert_eq!(
            model.entries[0].message_count,
            materialized[0].message_count
        );
        assert_eq!(session_model.entries.len(), 1);
        assert_eq!(
            session_model.entries[0].session_id.as_deref(),
            Some("chat-1")
        );
        assert_eq!(workspace_model.entries.len(), 1);
        assert_eq!(
            workspace_model.entries[0].workspace_key.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            workspace_model.entries[0].workspace_label.as_deref(),
            Some("workspace-a")
        );
        assert_eq!(
            monthly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            1
        );
        assert_eq!(
            hourly
                .entries
                .iter()
                .map(|entry| entry.message_count)
                .sum::<i32>(),
            1
        );
        assert_eq!(agents.total_messages, 1);
        assert_eq!(model.total_input, monthly.entries[0].input);
        assert_eq!(model.total_input, hourly.entries[0].input);
        assert_eq!(model.total_input, agents.entries[0].input);
        assert_eq!(model.total_output, monthly.entries[0].output);
        assert_eq!(model.total_output, hourly.entries[0].output);
        assert_eq!(model.total_output, agents.entries[0].output);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_warm_cache_mixed_hits_reapply_suppression_and_restore_snapshot() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "snapshot input", "snapshot output"),
        );
        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let home = source_home.path().to_str().unwrap();
        let clients = ["kiro".to_string()];

        let first = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // Snapshot hit + execution miss: only the newly parsed execution wins.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "succeed", "1770983426.5");
        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(streamed.len(), 1);
        assert_eq!(streamed[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // Execution hit + snapshot miss: the snapshot change is still suppressed.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body(
                "exec-1",
                "changed snapshot input",
                "changed snapshot output",
            ),
        );
        let second = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].dedup_key.as_deref(), Some("execution:exec-1"));

        // A failed rewrite does not erase usage already collected from a
        // successful execution, nor count its lower-authority snapshot again.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "failed", "1770983426");
        let mut failed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| failed.push(message.clone()),
        );
        assert_eq!(failed, second);

        // Restoring the same successful execution must not duplicate it.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let restored_execution = parse_all_messages_with_pricing_with_env_strategy(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            None,
        );
        assert_eq!(restored_execution.len(), 1);
        assert_eq!(
            restored_execution[0].dedup_key.as_deref(),
            Some("execution:exec-1")
        );
        assert!(message_cache::SourceMessageCache::load()
            .get(
                message_cache::CacheIdentity::for_client(ClientId::Kiro),
                &execution,
            )
            .is_some());

        // Source deletion keeps the collected authoritative execution rather
        // than replacing it with the lower-authority global snapshot.
        std::fs::remove_file(&execution).unwrap();
        let mut restored = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| restored.push(message.clone()),
        );
        assert_eq!(restored, restored_execution);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_suppression_precedes_report_date_filter() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-future", "snapshot input", "snapshot output"),
        );
        let execution = m15a_global_root(source_home.path())
            .join("workspace-a/execution-store/execution-future");
        std::fs::create_dir_all(execution.parent().unwrap()).unwrap();
        std::fs::write(
            &execution,
            r#"{"executionId":"exec-future","chatSessionId":"chat-future","status":"succeed","startTime":4102444800000,"endTime":4102444801000,"actions":[{"actionType":"say","output":"future answer"}],"input":{"data":{"messages":[{"content":"future question"}]}}}"#,
        )
        .unwrap();
        let snapshot_date = sessions::kiro::parse_kiro_file(&snapshot)[0].date.clone();

        let mut options = m15a_report_options(source_home.path());
        options.until = Some(snapshot_date);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let report = runtime.block_on(get_model_report(options)).unwrap();
        assert!(report.entries.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial_test::serial]
    fn m15a_globalstorage_mtime_pruning_and_stat_failure_fail_open() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
        ]);
        let snapshot = write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "initial input", "initial output"),
        );
        let options = m15a_local_options(source_home.path());
        let before = latest_source_mtime_ms(&options).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "rewritten input", "rewritten output"),
        );
        let after = latest_source_mtime_ms(&options).unwrap();
        assert!(after > before, "globalStorage primary mtime must advance");

        let parsed = parse_local_clients(LocalParseOptions {
            modified_after: Some(before + 1),
            ..options.clone()
        })
        .unwrap();
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);

        let execution = write_m15a_execution(source_home.path(), "succeed", "1770983426");
        let execution_mtime = super::kiro_source_mtime_ms(&execution).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_m15a_snapshot(
            source_home.path(),
            &m15a_snapshot_body("exec-1", "newer snapshot", "must stay suppressed"),
        );
        let snapshot_mtime = super::kiro_source_mtime_ms(&snapshot).unwrap();
        assert!(snapshot_mtime > execution_mtime);
        let snapshot_date = sessions::kiro::parse_kiro_file(&snapshot)[0].date.clone();
        let parsed = parse_local_clients(LocalParseOptions {
            modified_after: Some(execution_mtime + 1),
            since: Some(snapshot_date),
            ..options.clone()
        })
        .unwrap();
        assert_eq!(parsed.counts.get(ClientId::Kiro), 1);
        assert!(
            parsed.messages.is_empty(),
            "mtime pruning must retain the older execution until suppression"
        );

        let missing = source_home.path().join("missing.chat");
        let mut scan = scanner::ScanResult::default();
        scan.get_mut(ClientId::Kiro).push(missing);
        prune_scan_result_by_mtime(&mut scan, u64::MAX);
        assert_eq!(scan.get(ClientId::Kiro).len(), 1);

        // Keep the fixture path live for the cache/source identity assertion.
        assert!(message_cache::SourceFingerprint::from_kiro_path(&snapshot).is_some());
    }

    // ---- Batched parallel parse (PARSE_BATCH_SIZE) acceptance matrix ----
    // These fixtures deliberately exceed PARSE_BATCH_SIZE (32) so the driver
    // crosses at least one batch boundary, and compare the batched streaming
    // driver's output against the untouched, unbatched materialized path
    // (`parse_all_messages_with_pricing_with_env_strategy`), which shares the
    // exact same per-file cache/dedup semantics with an effectively unbounded
    // batch. Byte-identical output between the two is the direct test that
    // PARSE_BATCH_SIZE does not change results.

    fn write_claude_message(
        dir: &Path,
        file_name: &str,
        msg_id: &str,
        output_tokens: i64,
    ) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file_name);
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"message\":{{\"id\":\"{msg_id}\",\"model\":\"claude-sonnet-4\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":{output_tokens}}}}}}}\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    #[serial_test::serial]
    fn test_batched_claude_dedup_preserves_first_occurrence_across_batch_boundary() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let project_dir = source_home.path().join(".claude/projects/proj");

        // 40 files > PARSE_BATCH_SIZE(32), forcing two batches. File 0 (batch 1)
        // and file 39 (batch 2) share a dedup_key (same message id) with
        // different bodies (different output_tokens) — the cross-file gate
        // must keep file 0's occurrence regardless of batching.
        for i in 0..40 {
            let name = format!("f{i:02}.jsonl");
            if i == 0 {
                write_claude_message(&project_dir, &name, "dup-msg", 10);
            } else if i == 39 {
                write_claude_message(&project_dir, &name, "dup-msg", 999);
            } else {
                write_claude_message(&project_dir, &name, &format!("uniq-{i}"), i as i64);
            }
        }

        let clients = vec!["claude".to_string()];
        let mut streamed = Vec::new();
        with_isolated_tokscale_cache(cache_home.path(), || {
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |m: &UnifiedMessage| streamed.push(m.clone()),
            );
        });

        assert_eq!(streamed.len(), 39, "40 files, one duplicate dropped");
        let winner = streamed
            .iter()
            .find(|m| m.tokens.output == 10 || m.tokens.output == 999)
            .expect("the dup-msg occurrence must survive");
        assert_eq!(
            winner.tokens.output, 10,
            "file 0 (first path-order occurrence) must win, not file 39"
        );
        assert!(
            !streamed.iter().any(|m| m.tokens.output == 999),
            "file 39's body must not appear at all"
        );

        // Cross-check against the untouched materialized (unbounded-batch)
        // path over a fresh cache: same corpus, same dedup outcome.
        let mat_cache_home = tempfile::TempDir::new().unwrap();
        let materialized = with_isolated_tokscale_cache(mat_cache_home.path(), || {
            parse_all_messages_with_pricing(source_home.path().to_str().unwrap(), &clients, None)
        });
        assert_eq!(materialized.len(), streamed.len());
        assert_eq!(
            materialized
                .iter()
                .map(|m| &m.dedup_key)
                .collect::<Vec<_>>(),
            streamed.iter().map(|m| &m.dedup_key).collect::<Vec<_>>(),
            "batched streaming order must match the unbatched materialized order"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_batched_claude_hit_miss_interleave_within_batch() {
        let source_home = tempfile::TempDir::new().unwrap();
        let cache_home = tempfile::TempDir::new().unwrap();
        let project_dir = source_home.path().join(".claude/projects/proj");

        let mut paths = Vec::new();
        for i in 0..10 {
            let name = format!("f{i:02}.jsonl");
            paths.push(write_claude_message(
                &project_dir,
                &name,
                &format!("msg-{i}"),
                i as i64,
            ));
        }

        let clients = vec!["claude".to_string()];
        // Cache keys preserve the scanner's own path spelling, and no fixture
        // can reliably predict it: `TempDir::join` produces `\` on Windows
        // while the resolver builds `.claude/projects` with `/`, and WalkDir
        // then appends `\` again for the project subdirectory. Ask the scanner
        // what it calls each file instead of guessing — a wrong key stores an
        // entry the lookup can never find, and every intended hit silently
        // degrades into a fresh parse.
        let scan_result = scan_test_sources(
            source_home.path(),
            &clients,
            &scanner::ScannerSettings::default(),
        );
        let scanned = scan_result.get(ClientId::Claude);
        let cache_sources: Vec<PathBuf> = paths
            .iter()
            .map(|path| scanner_spelling(scanned, path))
            .collect();

        // Pre-warm the cache for even-indexed files only, with a sentinel body
        // that differs from what a fresh parse of the on-disk file would
        // produce, so a hit is distinguishable from a miss.
        with_isolated_tokscale_cache(cache_home.path(), || {
            let mut cache = message_cache::SourceMessageCache::default();
            for (i, path) in cache_sources.iter().enumerate() {
                if i % 2 != 0 {
                    continue;
                }
                let fingerprint =
                    message_cache::SourceFingerprint::from_claude_code_path_with_home(
                        path,
                        Some(source_home.path()),
                    )
                    .unwrap();
                let sentinel = UnifiedMessage::new_with_dedup(
                    "claude",
                    "cached-sentinel-model",
                    "anthropic",
                    "cached-session",
                    1_767_225_600_000,
                    TokenBreakdown {
                        input: 4242,
                        output: 1,
                        ..Default::default()
                    },
                    0.0,
                    Some(format!("sentinel-{i}")),
                );
                cache.insert(message_cache::CachedSourceEntry::new(
                    message_cache::CacheIdentity::for_client(ClientId::Claude)
                        .with_scope(source_home.path()),
                    path,
                    fingerprint,
                    vec![sentinel],
                    Vec::new(),
                    None,
                ));
            }
            cache.save_if_dirty();
        });

        // Separate the two ways a seeded hit can fail to replay, so a red run
        // names its own cause instead of leaving it to be guessed: a key the
        // lookup never finds, or an entry found but judged stale.
        with_isolated_tokscale_cache(cache_home.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            let entry = cache.get(
                message_cache::CacheIdentity::for_client(ClientId::Claude)
                    .with_scope(source_home.path()),
                &cache_sources[0],
            );
            assert!(
                entry.is_some(),
                "seeded entry must be reachable under the scanner's key spelling"
            );
            let fresh = message_cache::SourceFingerprint::from_claude_code_path_with_home(
                &cache_sources[0],
                Some(source_home.path()),
            )
            .unwrap();
            assert_eq!(
                entry.unwrap().fingerprint,
                fresh,
                "seeded fingerprint must still match a freshly computed one"
            );
        });

        let mut streamed = Vec::new();
        with_isolated_tokscale_cache(cache_home.path(), || {
            scan_messages_streaming(
                source_home.path().to_str().unwrap(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |m: &UnifiedMessage| streamed.push(m.clone()),
            );
        });

        assert_eq!(streamed.len(), 10);
        for (i, message) in streamed.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(
                    message.model_id, "cached-sentinel-model",
                    "file {i} must replay the cache hit"
                );
                assert_eq!(message.tokens.input, 4242);
            } else {
                assert_eq!(
                    message.model_id, "claude-sonnet-4",
                    "file {i} must be freshly parsed (miss)"
                );
                assert_eq!(message.tokens.output, i as i64);
            }
        }
    }

    fn codex_session_meta_line(session_id: &str, timestamp: &str) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"session_meta","payload":{{"id":"{session_id}","source":"interactive","model_provider":"openai"}}}}"#
        )
    }

    fn codex_turn_context_line(model: &str) -> String {
        format!(r#"{{"type":"turn_context","payload":{{"model":"{model}"}}}}"#)
    }

    fn codex_token_count_line(input: i64, cached: i64, output: i64) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output}}}}}}}}}"#
        )
    }

    /// A well-formed codex session: session_meta + turn_context + one
    /// token_count event carrying a cumulative total (drives the
    /// total-usage dedup key).
    fn codex_plain_fixture(
        session_id: &str,
        model: &str,
        timestamp: &str,
        input: i64,
        output: i64,
    ) -> String {
        format!(
            "{}\n{}\n{}\n",
            codex_session_meta_line(session_id, timestamp),
            codex_turn_context_line(model),
            codex_token_count_line(input, 0, output)
        )
    }

    /// Trailing malformed JSON after valid lines: consumed_offset ends up
    /// short of the file size, so `build_codex_cache_entry` never persists
    /// an entry (the "uncacheable" cache-matrix case).
    fn codex_uncacheable_fixture(session_id: &str, model: &str, timestamp: &str) -> String {
        format!(
            "{}\n{}\n{}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"info\":{{\"total_token_usage\":{{\"input_tokens\":999",
            codex_session_meta_line(session_id, timestamp),
            codex_turn_context_line(model),
            codex_token_count_line(1, 0, 1),
        )
    }

    fn write_codex_session(dir: &Path, file_name: &str, content: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(file_name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Full legacy-compatible cache/incremental matrix spanning the former
    /// batch boundary. Malformed sources retain explicit materialized-prefix
    /// semantics; production synchronization must reject the same corpus
    /// atomically instead of publishing that prefix.
    #[test]
    #[serial_test::serial]
    fn test_batched_codex_cache_matrix_across_batch_boundary() {
        let stream_home = tempfile::TempDir::new().unwrap();
        let stream_cache = tempfile::TempDir::new().unwrap();
        let mat_home = tempfile::TempDir::new().unwrap();
        let mat_cache = tempfile::TempDir::new().unwrap();

        let stream_dir = scanner_fixture_path(stream_home.path(), ".codex/sessions");
        let mat_dir = scanner_fixture_path(mat_home.path(), ".codex/sessions");

        // Build the identical 36-file corpus in both homes.
        // idx 0: hit;            idx 1: append-resume;   idx 2: stale reparse
        // idx 3: uncacheable;    idx 4: cached-then-invalidated
        // idx 5..29: filler (25 plain unique files)
        // idx 30: hit;           idx 31: dup partner A (last of batch 1)
        // idx 32: dup partner B (first of batch 2, same key, diff body)
        // idx 33: append-resume; idx 34: stale reparse; idx 35: cached-then-invalidated
        let build = |dir: &Path| {
            for i in 0..36 {
                let name = format!("f{i:02}.jsonl");
                let content = match i {
                    0 | 30 => codex_plain_fixture(
                        &format!("sess-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                        10,
                        5,
                    ),
                    1 | 33 => codex_plain_fixture(
                        &format!("sess-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                        10,
                        5,
                    ),
                    2 | 34 => codex_plain_fixture(
                        &format!("sess-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                        10,
                        5,
                    ),
                    3 => codex_uncacheable_fixture(
                        &format!("sess-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                    ),
                    4 | 35 => codex_plain_fixture(
                        &format!("sess-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                        10,
                        5,
                    ),
                    31 => {
                        codex_plain_fixture("sess-dup", "gpt-5.4", "2026-01-01T00:00:00Z", 77, 33)
                    }
                    32 => {
                        codex_plain_fixture("sess-dup", "gpt-5.4", "2026-01-01T09:00:00Z", 77, 33)
                    }
                    _ => codex_plain_fixture(
                        &format!("sess-filler-{i}"),
                        "gpt-5.4",
                        "2026-01-01T00:00:00Z",
                        i as i64,
                        1,
                    ),
                };
                write_codex_session(dir, &name, &content);
            }
        };
        build(&stream_dir);
        build(&mat_dir);

        let clients = vec!["codex".to_string()];

        let reject_incomplete_production = || {
            with_isolated_tokscale_cache(stream_cache.path(), || {
                let mut emitted = Vec::new();
                let result = super::scan_messages_streaming(
                    stream_home.path().to_str().unwrap(),
                    &clients,
                    None,
                    false,
                    &scanner::ScannerSettings::default(),
                    &|_| true,
                    &mut |message| emitted.push(message.clone()),
                );
                assert!(
                    result.is_err(),
                    "incomplete source must reject the whole production scope"
                );
                assert!(
                    emitted.is_empty(),
                    "failed source synchronization must not emit a prefix"
                );
            });
        };
        reject_incomplete_production();

        // Cold pass on both.
        let mut cold_streamed = Vec::new();
        with_isolated_tokscale_cache(stream_cache.path(), || {
            super::scan_messages_disk(
                None,
                stream_home.path(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
                super::LocalUsageSelection::UnifiedCompatibility,
                &|_| true,
                &mut |m: &UnifiedMessage| cold_streamed.push(m.clone()),
            )
            .unwrap();
        });
        let cold_materialized = with_isolated_tokscale_cache(mat_cache.path(), || {
            parse_all_messages_with_pricing(mat_home.path().to_str().unwrap(), &clients, None)
        });

        assert_eq!(cold_streamed.len(), cold_materialized.len());
        assert_eq!(
            cold_streamed
                .iter()
                .map(|m| &m.dedup_key)
                .collect::<Vec<_>>(),
            cold_materialized
                .iter()
                .map(|m| &m.dedup_key)
                .collect::<Vec<_>>(),
            "cold: batched streaming order must match the unbatched materialized order"
        );
        // Only one of the sess-dup pair (idx 31 vs 32) survived, and it is
        // idx 31's body (earlier path order): input=77 must appear once.
        assert_eq!(
            cold_streamed
                .iter()
                .filter(|m| m.tokens.input == 77)
                .count(),
            1
        );
        // idx 3's uncacheable file never persisted a cache entry.
        with_isolated_tokscale_cache(stream_cache.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            // Prove the lookup spelling first: a wrong path key would make the
            // `is_none()` below pass for the wrong reason.
            assert!(
                cache
                    .get(
                        message_cache::CacheIdentity::for_client(ClientId::Codex),
                        &stream_dir.join("f00.jsonl"),
                    )
                    .is_some(),
                "a cacheable sibling must be found, or the key spelling is wrong"
            );
            assert!(cache
                .get(
                    message_cache::CacheIdentity::for_client(ClientId::Codex),
                    &stream_dir.join("f03.jsonl"),
                )
                .is_none());
        });

        // Mutate for the warm pass, identically in both homes:
        // idx 1/33 — append a valid extra token_count event (append-resume).
        // idx 2/34 — full rewrite with different leading bytes (stale prefix -> full reparse).
        // idx 4/35 — rewrite to malformed trailing JSON (cached-then-invalidated).
        let mutate = |dir: &Path| {
            for i in [1usize, 33] {
                let path = dir.join(format!("f{i:02}.jsonl"));
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                file.write_all(codex_token_count_line(20, 0, 8).as_bytes())
                    .unwrap();
                file.write_all(b"\n").unwrap();
                file.flush().unwrap();
            }
            for i in [2usize, 34] {
                let path = dir.join(format!("f{i:02}.jsonl"));
                std::fs::write(
                    &path,
                    codex_plain_fixture(
                        &format!("sess-{i}-rewritten"),
                        "gpt-5.5",
                        "2026-02-02T00:00:00Z",
                        44,
                        22,
                    ),
                )
                .unwrap();
            }
            for i in [4usize, 35] {
                let path = dir.join(format!("f{i:02}.jsonl"));
                std::fs::write(
                    &path,
                    codex_uncacheable_fixture(
                        &format!("sess-{i}-broken"),
                        "gpt-5.4",
                        "2026-02-02T00:00:00Z",
                    ),
                )
                .unwrap();
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(5));
        mutate(&stream_dir);
        mutate(&mat_dir);
        reject_incomplete_production();

        let mut warm_streamed = Vec::new();
        with_isolated_tokscale_cache(stream_cache.path(), || {
            super::scan_messages_disk(
                None,
                stream_home.path(),
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                None,
                super::LocalUsageSelection::UnifiedCompatibility,
                &|_| true,
                &mut |m: &UnifiedMessage| warm_streamed.push(m.clone()),
            )
            .unwrap();
        });
        let warm_materialized = with_isolated_tokscale_cache(mat_cache.path(), || {
            parse_all_messages_with_pricing(mat_home.path().to_str().unwrap(), &clients, None)
        });

        assert_eq!(warm_streamed.len(), warm_materialized.len());
        assert_eq!(
            warm_streamed
                .iter()
                .map(|m| &m.dedup_key)
                .collect::<Vec<_>>(),
            warm_materialized
                .iter()
                .map(|m| &m.dedup_key)
                .collect::<Vec<_>>(),
            "warm: batched streaming order must match the unbatched materialized order"
        );
        assert_eq!(
            warm_streamed.iter().map(|m| m.tokens.input).collect::<Vec<_>>(),
            warm_materialized.iter().map(|m| m.tokens.input).collect::<Vec<_>>(),
            "warm: per-message token totals must match exactly (hit/append-resume/reparse/invalidate all agree)"
        );

        // idx 4/35 previously cached, now invalidated: cache entry removed.
        with_isolated_tokscale_cache(stream_cache.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            for i in [4, 35] {
                assert!(
                    cache
                        .get(
                            message_cache::CacheIdentity::for_client(ClientId::Codex),
                            &stream_dir.join(format!("f{i:02}.jsonl")),
                        )
                        .is_none(),
                    "file {i} must have its stale cache entry removed, not just left stale"
                );
            }
        });
        // idx 1/33 append-resume: incremental offset now covers the appended bytes.
        with_isolated_tokscale_cache(stream_cache.path(), || {
            let cache = message_cache::SourceMessageCache::load();
            for i in [1, 33] {
                let path = stream_dir.join(format!("f{i:02}.jsonl"));
                let entry = cache
                    .get(
                        message_cache::CacheIdentity::for_client(ClientId::Codex),
                        &path,
                    )
                    .expect("append-resumed file must still be cached");
                let incremental = entry
                    .codex_incremental
                    .as_ref()
                    .expect("append-resume must persist incremental state");
                assert_eq!(
                    incremental.consumed_offset,
                    std::fs::metadata(&path).unwrap().len(),
                    "file {i} incremental offset must cover the full appended file"
                );
            }
        });
    }

    // ---- ZCode v2 SQLite: sibling (`-wal`) rule ----
    // ZCode's usage database is SQLite held in the single `zcode_db` field. A
    // WAL-mode writer appends to `db.sqlite-wal` without touching the main
    // file, so every lane must treat the `-wal` sidecar as part of the source.

    fn zcode_wal_db(source_home: &Path) -> (PathBuf, rusqlite::Connection) {
        let db_dir = source_home.join(".zcode/cli/db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("db.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode=WAL;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        conn.execute_batch(
            "PRAGMA wal_autocheckpoint=0;
             CREATE TABLE model_usage (
                 id TEXT PRIMARY KEY, session_id TEXT, turn_id TEXT, model_id TEXT,
                 started_at INTEGER, completed_at INTEGER, duration_ms INTEGER,
                 input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
                 cache_read_input_tokens INTEGER, cache_creation_input_tokens INTEGER,
                 computed_total_tokens INTEGER, agent TEXT, mode TEXT
             );
             CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, path TEXT);
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
        (db_path, conn)
    }

    fn insert_zcode_usage(conn: &rusqlite::Connection, id: &str, started_at: i64, input: i64) {
        conn.execute(
            "INSERT INTO model_usage (id, session_id, turn_id, model_id, started_at,
                 input_tokens, output_tokens, computed_total_tokens)
             VALUES (?1, 'sess_1', ?1, 'glm-5.2', ?2, ?3, 10, ?3 + 10)",
            rusqlite::params![id, started_at, input],
        )
        .unwrap();
    }

    /// Main-file (len, mtime): unchanged across a write proves it went to the WAL only.
    fn main_file_state(path: &Path) -> (u64, std::time::SystemTime) {
        let meta = std::fs::metadata(path).unwrap();
        (meta.len(), meta.modified().unwrap())
    }

    type ZcodeShape = (
        Option<String>,
        i64,
        TokenBreakdown,
        bool,
        Option<String>,
        Option<String>,
    );

    fn zcode_message_shape(messages: &[UnifiedMessage]) -> Vec<ZcodeShape> {
        let mut shape: Vec<_> = messages
            .iter()
            .map(|m| {
                (
                    m.dedup_key.clone(),
                    m.timestamp,
                    m.tokens.clone(),
                    m.is_turn_start,
                    m.workspace_key.clone(),
                    m.agent.clone(),
                )
            })
            .collect();
        shape.sort_by(|a, b| a.0.cmp(&b.0));
        shape
    }

    #[test]
    #[serial_test::serial]
    fn test_zcode_materialized_rebuilds_on_wal_only_write() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path().to_str().unwrap();
        let clients = ["zcode".to_string()];

        let (db_path, conn) = zcode_wal_db(source_home.path());
        insert_zcode_usage(&conn, "usage_1", 1_782_718_000_000, 100);
        assert_eq!(
            parse_all_messages_with_pricing(home, &clients, None).len(),
            1
        );

        let before = main_file_state(&db_path);
        insert_zcode_usage(&conn, "usage_2", 1_782_718_100_000, 200);
        assert_eq!(main_file_state(&db_path), before, "write must be WAL-only");

        let refreshed = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(
            refreshed.len(),
            2,
            "a WAL-only write must change the zcode fingerprint and force a re-parse"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_zcode_streaming_matches_materialized_after_wal_only_write() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path().to_str().unwrap();
        let clients = ["zcode".to_string()];
        let stream = || {
            let mut streamed = Vec::new();
            scan_messages_streaming(
                home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |message| streamed.push(message.clone()),
            );
            streamed
        };

        let (db_path, conn) = zcode_wal_db(source_home.path());
        insert_zcode_usage(&conn, "usage_1", 1_782_718_000_000, 100);
        assert_eq!(stream().len(), 1);

        let before = main_file_state(&db_path);
        let options = LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        };
        let source_token = crate::local_source_change_token(&options).unwrap();
        insert_zcode_usage(&conn, "usage_2", 1_782_718_100_000, 200);
        assert_eq!(main_file_state(&db_path), before, "write must be WAL-only");
        assert_ne!(
            crate::local_source_change_token(&options).unwrap(),
            source_token,
            "committed WAL-only usage must invalidate live source demand"
        );

        let streamed = stream();
        assert_eq!(
            streamed.len(),
            2,
            "the streaming lane must not serve the pre-WAL cache entry"
        );
        let materialized = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(
            zcode_message_shape(&streamed),
            zcode_message_shape(&materialized)
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Zcode), 2);
        assert_eq!(counted.messages.len(), 2);
    }

    #[test]
    #[serial_test::serial]
    fn test_modified_after_keeps_zcode_db_when_only_wal_is_newer() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());

        // `conn` stays open: closing the last connection checkpoints and
        // removes the WAL.
        let (db_path, conn) = zcode_wal_db(source_home.path());
        insert_zcode_usage(&conn, "usage_1", 1_782_718_000_000, 100);
        insert_zcode_usage(&conn, "usage_2", 1_782_718_100_000, 200);

        let db_time =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let db_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&db_path)
            .unwrap();
        let Ok(()) = db_file.set_modified(db_time) else {
            return;
        };
        drop(db_file);
        let threshold_ms = 1_700_086_400_000;
        let wal_mtime = std::fs::metadata(db_path.with_file_name("db.sqlite-wal"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            wal_mtime > std::time::UNIX_EPOCH + std::time::Duration::from_millis(threshold_ms),
            "the WAL must be the newer sibling"
        );

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["zcode".to_string()]),
            modified_after: Some(threshold_ms),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            counted.counts.get(ClientId::Zcode),
            2,
            "an old db.sqlite with a newer -wal must survive modified-after pruning"
        );
    }

    // ---- Muse (upstream `d409ee07` + #1355): nested discovery, pricing, dedup ----
    // Adapted from upstream `tests/muse.rs`'s end-to-end test: a parent session
    // and its `subagent/<uuid>/` transcript are separate sessions; a byte copy
    // of the parent under another date directory collapses on its stream key.
    // Every lane must agree, and the priced cost must match the split buckets
    // (cache reads out of input, reasoning out of output).
    #[test]
    #[serial_test::serial]
    fn test_muse_nested_sessions_price_and_dedup_in_every_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", cache_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.path().as_os_str()),
            // Hermetic durable root: outranks TOKSCALE_CONFIG_DIR after
            // canonicalization; keeps the clear_cache store path in-profile.
            (
                "SYRTIS_USAGE_DIR",
                cache_home.path().join("usage").as_os_str(),
            ),
        ]);
        const PARENT: &str = "01a0b7d1-b7af-7de2-8369-4f4815ae1d72";
        const CHILD: &str = "01a0b7df-bac6-75d2-b28e-8dab127205f0";
        let usage_line = |session: &str, sequence: i64, recorded_at: i64, usage: &str| {
            format!(
                r#"{{"schema_version":1,"stream":{{"kind":"session","id":"{session}"}},"sequence":{sequence},"recorded_at":{recorded_at},"record_type":"event","payload_type":"runtime.session","payload_schema_version":1,"payload":{{"kind":"run","event":{{"kind":"model_completed","usage":{usage},"duration_ms":1000,"finish_reason":"tool_calls","model":"muse-spark-1.3-contributor"}}}}}}"#
            )
        };
        let sessions = source_home.path().join(".local/share/muse/sessions");
        let parent_line = usage_line(
            PARENT,
            39,
            1789790455896395,
            r#"{"input_tokens":1000,"output_tokens":250,"cached_tokens":400,"cache_write_tokens":50,"cache_read_tokens":400,"reasoning_tokens":25}"#,
        );
        for date in ["2026/09/18", "2026/09/19"] {
            let dir = sessions.join(date).join(PARENT);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("session.jsonl"), &parent_line).unwrap();
        }
        let child_dir = sessions
            .join("2026/09/18")
            .join(PARENT)
            .join("subagent")
            .join(CHILD);
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::write(
            child_dir.join("session.jsonl"),
            usage_line(
                CHILD,
                40,
                1789791288021470,
                r#"{"input_tokens":100,"output_tokens":10,"cached_tokens":0,"cache_write_tokens":0,"cache_read_tokens":0,"reasoning_tokens":0}"#,
            ),
        )
        .unwrap();

        let mut litellm = HashMap::new();
        litellm.insert(
            "muse-spark-1.3-contributor".to_string(),
            pricing::ModelPricing {
                input_cost_per_token: Some(0.001),
                output_cost_per_token: Some(0.002),
                cache_read_input_token_cost: Some(0.0001),
                cache_creation_input_token_cost: Some(0.0005),
                ..Default::default()
            },
        );
        let pricing_service = pricing::PricingService::new(litellm, HashMap::new());
        let parent_cost = 600.0 * 0.001 + (225.0 + 25.0) * 0.002 + 400.0 * 0.0001 + 50.0 * 0.0005;
        let home = source_home.path().to_str().unwrap();
        let clients = ["muse".to_string()];
        let check = |messages: Vec<UnifiedMessage>, lane: &str| {
            assert_eq!(messages.len(), 2, "{lane}");
            let parent = messages
                .iter()
                .find(|m| m.session_id == PARENT)
                .unwrap_or_else(|| panic!("{lane}: parent missing"));
            assert_eq!(
                (
                    parent.tokens.input,
                    parent.tokens.cache_read,
                    parent.tokens.output,
                    parent.tokens.reasoning,
                    parent.tokens.cache_write
                ),
                (600, 400, 225, 25, 50),
                "{lane}"
            );
            assert_eq!(parent.provider_id, "meta", "{lane}");
            assert!((parent.cost - parent_cost).abs() < 1e-10, "{lane}");
            assert!(messages.iter().any(|m| m.session_id == CHILD), "{lane}");
        };

        // Each lane runs cold (cache cleared first) and then warm, so both the
        // parse-and-writeback and the cache-hit paths are exercised per lane.
        let clear_cache = |expect_written: bool| {
            // Durable facts live under the canonical usage root, never the
            // legacy cache dir.
            let store = crate::paths::get_usage_dir().join("source-message-cache.sqlite3");
            assert_eq!(store.exists(), expect_written, "{store:?}");
            if expect_written {
                std::fs::remove_file(&store).unwrap();
            }
        };
        clear_cache(false);
        for pass in ["cold", "warm"] {
            check(
                parse_all_messages_with_pricing_with_env_strategy(
                    home,
                    &clients,
                    Some(&pricing_service),
                    false,
                    &scanner::ScannerSettings::default(),
                    None,
                ),
                &format!("materialized, {pass}"),
            );
        }
        clear_cache(true);
        for pass in ["cold", "warm"] {
            let mut streamed = Vec::new();
            scan_messages_streaming(
                home,
                &clients,
                Some(&pricing_service),
                false,
                &scanner::ScannerSettings::default(),
                &|_m: &UnifiedMessage| true,
                &mut |m: &UnifiedMessage| streamed.push(m.clone()),
            );
            check(streamed, &format!("streaming, {pass}"));
        }

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Muse), 2);
        assert_eq!(counted.messages.len(), 2);
    }

    // ---- Augment: a session snapshot copied to a second file counts once ----
    // Auggie keys every turn as `augment:<sessionId>:req:<requestId>`, so a
    // second snapshot of the same session (a backup copy, a renamed file) must
    // collapse in the materialized, streaming and count lanes alike.
    #[test]
    #[serial_test::serial]
    fn test_augment_duplicate_session_snapshot_counts_once_in_every_lane() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let sessions_dir = source_home.path().join(".augment/sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session = r#"{
            "sessionId": "sess-aug",
            "agentState": { "modelId": "claude-sonnet-4-5" },
            "chatHistory": [
                { "completed": true, "finishedAt": "2026-01-15T12:01:00.000Z", "sequenceId": 1,
                  "exchange": { "request_id": "req-1", "response_nodes": [
                      { "token_usage": { "input_tokens": 100, "output_tokens": 10,
                                         "cache_read_input_tokens": 50,
                                         "cache_creation_input_tokens": 5 } } ] } },
                { "completed": true, "finishedAt": "2026-01-15T12:02:00.000Z", "sequenceId": 2,
                  "exchange": { "request_id": "req-2", "response_nodes": [
                      { "token_usage": { "input_tokens": 200, "output_tokens": 20 } } ] } }
            ]
        }"#;
        std::fs::write(sessions_dir.join("sess-aug.json"), session).unwrap();
        std::fs::write(sessions_dir.join("sess-aug-copy.json"), session).unwrap();
        let home = source_home.path().to_str().unwrap();
        let clients = ["augment".to_string()];

        let shape = |messages: &[UnifiedMessage]| {
            let mut shape: Vec<_> = messages
                .iter()
                .map(|m| (m.dedup_key.clone(), m.timestamp, m.tokens.clone()))
                .collect();
            shape.sort_by(|a, b| a.0.cmp(&b.0));
            shape
        };

        let materialized = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(materialized.len(), 2, "the copied snapshot must collapse");

        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(shape(&streamed), shape(&materialized));

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Augment), 2);
        assert_eq!(counted.messages.len(), 2);
    }

    // ---- Hindsight: ledger rows across monthly files, three lanes ----
    // A row re-synced into a second monthly file shares its request id and
    // must count once; OpenAI-shape rows must sum to their stated totals.
    #[test]
    #[serial_test::serial]
    fn test_hindsight_ledger_lanes_agree_and_conserve_stated_totals() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let usage_dir = source_home.path().join(".hindsight/usage");
        std::fs::create_dir_all(&usage_dir).unwrap();
        let row_a = r#"{"id":"a","trace_id":"t1","provider":"openai","model":"openai/gpt-6-luna","started_at":"2026-09-30T23:59:00+00:00","input_tokens":3939,"output_tokens":620,"cached_tokens":2618,"total_tokens":4559,"bank":"hermes"}"#;
        let row_b = r#"{"id":"b","trace_id":"t2","provider":"openai","model":"openai/gpt-6-luna","started_at":"2026-10-01T08:26:27+00:00","input_tokens":1000,"output_tokens":100,"cached_tokens":null,"total_tokens":1100,"bank":"hermes"}"#;
        std::fs::write(usage_dir.join("2026-09.jsonl"), format!("{row_a}\n")).unwrap();
        std::fs::write(
            usage_dir.join("2026-10.jsonl"),
            format!("{row_a}\n{row_b}\n"),
        )
        .unwrap();
        let home = source_home.path().to_str().unwrap();
        let clients = ["hindsight".to_string()];

        let shape = |messages: &[UnifiedMessage]| {
            let mut shape: Vec<_> = messages
                .iter()
                .map(|m| (m.dedup_key.clone(), m.timestamp, m.tokens.clone()))
                .collect();
            shape.sort_by(|a, b| a.0.cmp(&b.0));
            shape
        };

        let materialized = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(materialized.len(), 2, "the re-synced row must count once");
        let total: i64 = materialized.iter().map(|m| m.tokens.total()).sum();
        assert_eq!(total, 4559 + 1100, "lanes must sum to the stated totals");

        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            &clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |message| streamed.push(message.clone()),
        );
        assert_eq!(shape(&streamed), shape(&materialized));

        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.counts.get(ClientId::Hindsight), 2);
        let counted_total: i64 = counted
            .messages
            .iter()
            .map(|m| m.input + m.output + m.cache_read + m.cache_write + m.reasoning)
            .sum();
        assert_eq!(counted_total, 4559 + 1100);
    }

    // ---- Reasonix stats: three lanes, request counts, appends ----
    fn write_reasonix_stats(source_home: &Path, file: &str, lines: &[&str]) {
        // The platform default root (Windows: AppData/Roaming/reasonix).
        let stats_dir = PathBuf::from(
            ClientId::Reasonix
                .data()
                .resolve_path_with_env_strategy(source_home.to_str().unwrap(), false),
        );
        std::fs::create_dir_all(&stats_dir).unwrap();
        std::fs::write(stats_dir.join(file), format!("{}\n", lines.join("\n"))).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn test_reasonix_stats_lanes_agree_and_reparse_appends() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let first = r#"{"ts":"2026-08-04T09:10:11Z","model":"deepseek/chat","prompt":100,"completion":20,"cache_hit":30,"cache_miss":70,"total":120,"requests":3}"#;
        let second = r#"{"ts":"2026-08-04T09:11:11Z","model":"deepseek/chat","prompt":10,"completion":2,"total":12}"#;
        write_reasonix_stats(source_home.path(), "2026-08-04.jsonl", &[first]);
        let home = source_home.path().to_str().unwrap();
        let clients = ["reasonix".to_string()];
        let shape = |messages: &[UnifiedMessage]| {
            let mut shape: Vec<_> = messages
                .iter()
                .map(|m| (m.dedup_key.clone(), m.message_count, m.tokens.clone()))
                .collect();
            shape.sort_by(|a, b| a.0.cmp(&b.0));
            shape
        };
        let streamed = || {
            let mut streamed = Vec::new();
            scan_messages_streaming(
                home,
                &clients,
                None,
                false,
                &scanner::ScannerSettings::default(),
                &|_| true,
                &mut |message| streamed.push(message.clone()),
            );
            streamed
        };
        let counted = || {
            parse_local_clients(LocalParseOptions {
                home_dir: Some(home.to_string()),
                use_env_roots: false,
                clients: Some(clients.to_vec()),
                ..Default::default()
            })
            .unwrap()
        };

        let materialized = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(materialized.len(), 1);
        assert_eq!(materialized[0].tokens.total(), 120);
        assert_eq!(shape(&streamed()), shape(&materialized));
        assert_eq!(counted().counts.get(ClientId::Reasonix), 3);

        // Appending a record re-parses the warm-cached daily file.
        write_reasonix_stats(source_home.path(), "2026-08-04.jsonl", &[first, second]);
        let materialized = parse_all_messages_with_pricing(home, &clients, None);
        assert_eq!(materialized.len(), 2);
        assert_eq!(shape(&streamed()), shape(&materialized));
        let counted = counted();
        assert_eq!(counted.counts.get(ClientId::Reasonix), 4);
        assert_eq!(counted.messages.len(), 2);
    }

    #[test]
    #[serial_test::serial]
    fn test_reasonix_request_count_saturates_instead_of_overflowing() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let huge = r#"{"ts":"2026-08-04T09:10:11Z","model":"deepseek/chat","prompt":1,"completion":1,"total":2,"requests":9999999999}"#;
        write_reasonix_stats(source_home.path(), "2026-08-04.jsonl", &[huge, huge]);
        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["reasonix".to_string()]),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(counted.messages.len(), 2);
        assert_eq!(counted.counts.get(ClientId::Reasonix), i32::MAX);

        // The report folds saturate too instead of panicking on overflow.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let options = ReportOptions {
            home_dir: Some(source_home.path().to_str().unwrap().to_string()),
            use_env_roots: false,
            clients: Some(vec!["reasonix".to_string()]),
            ..Default::default()
        };
        let model = runtime.block_on(get_model_report(options.clone())).unwrap();
        assert_eq!(model.total_messages, i32::MAX);
        let monthly = runtime
            .block_on(get_monthly_report(options.clone()))
            .unwrap();
        assert_eq!(monthly.entries[0].message_count, i32::MAX);
        let hourly = runtime
            .block_on(get_hourly_report(options.clone()))
            .unwrap();
        assert_eq!(hourly.entries[0].message_count, i32::MAX);
        let agents = runtime.block_on(get_agents_report(options)).unwrap();
        assert_eq!(agents.total_messages, i32::MAX);
    }

    // ---- Kimchi: Pi format under its own root; pair fixtures ----
    fn write_pi_format_session(dir: &Path, file: &str, session: &str, message_id: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(file),
            format!(
                "{}\n{}\n",
                format_args!(
                    r#"{{"type":"session","id":"{session}","timestamp":"2026-08-01T12:00:00.000Z","cwd":"/tmp/p"}}"#
                ),
                format_args!(
                    r#"{{"type":"message","id":"{message_id}","timestamp":"2026-08-01T12:00:01.000Z","message":{{"role":"assistant","model":"kimi-k2.6","provider":"kimchi-dev","usage":{{"input":100,"output":10}}}}}}"#
                ),
            ),
        )
        .unwrap();
    }

    /// (client, session) of every message, per lane, sorted.
    fn client_sessions_per_lane(home: &str, clients: &[String]) -> [Vec<(String, String)>; 3] {
        let sorted = |mut v: Vec<(String, String)>| {
            v.sort();
            v
        };
        let materialized = parse_all_messages_with_pricing(home, clients, None)
            .into_iter()
            .map(|m| (m.client, m.session_id))
            .collect();
        let mut streamed = Vec::new();
        scan_messages_streaming(
            home,
            clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |m| streamed.push((m.client.clone(), m.session_id.clone())),
        );
        let counted = parse_local_clients(LocalParseOptions {
            home_dir: Some(home.to_string()),
            use_env_roots: false,
            clients: Some(clients.to_vec()),
            ..Default::default()
        })
        .unwrap()
        .messages
        .into_iter()
        .map(|m| (m.client, m.session_id))
        .collect();
        [sorted(materialized), sorted(streamed), sorted(counted)]
    }

    #[test]
    #[serial_test::serial]
    fn test_kimchi_and_pi_each_count_only_their_own_root() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path();
        write_pi_format_session(
            &home.join(".pi/agent/sessions/--p--"),
            "a.jsonl",
            "pi_ses",
            "m1",
        );
        write_pi_format_session(
            &home.join(".config/kimchi/harness/sessions/--p--"),
            "a.jsonl",
            "kimchi_ses",
            "m1",
        );
        // A copy of the Kimchi session file counts once (same session, same id).
        write_pi_format_session(
            &home.join(".config/kimchi/harness/sessions/--p--"),
            "copy.jsonl",
            "kimchi_ses",
            "m1",
        );

        let clients = ["pi".to_string(), "kimchi".to_string()];
        let expected = vec![
            ("kimchi".to_string(), "kimchi_ses".to_string()),
            ("pi".to_string(), "pi_ses".to_string()),
        ];
        for lane in client_sessions_per_lane(home.to_str().unwrap(), &clients) {
            assert_eq!(lane, expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_pi_format_files_under_the_cline_cli_root_reach_no_lane() {
        // Upstream's Kimchi commit also added standalone Cline CLI sessions
        // (`~/.cline/data/sessions`); that half is not vendored, so a Pi-format
        // file there must not be read by Cline, Kimchi or Pi.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path();
        write_pi_format_session(
            &home.join(".cline/data/sessions"),
            "s.jsonl",
            "cline_cli_ses",
            "m1",
        );
        write_pi_format_session(
            &home.join(".config/kimchi/harness/sessions/--p--"),
            "a.jsonl",
            "kimchi_ses",
            "m1",
        );

        let clients = ["pi".to_string(), "kimchi".to_string(), "cline".to_string()];
        for lane in client_sessions_per_lane(home.to_str().unwrap(), &clients) {
            assert_eq!(lane, vec![("kimchi".to_string(), "kimchi_ses".to_string())]);
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_senpi_and_pi_each_count_only_their_own_root_and_forks_collapse() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path();
        write_pi_format_session(
            &home.join(".pi/agent/sessions/--p--"),
            "a.jsonl",
            "pi_ses",
            "m1",
        );
        write_pi_format_session(
            &home.join(".senpi/agent/sessions/--p--"),
            "a.jsonl",
            "senpi_ses",
            "m1",
        );
        // A fork copies the parent's records into a new session file; the
        // cross-session key counts the copied record once.
        write_pi_format_session(
            &home.join(".senpi/agent/sessions/--p--"),
            "fork.jsonl",
            "senpi_fork",
            "m1",
        );

        let clients = ["pi".to_string(), "senpi".to_string()];
        for lane in client_sessions_per_lane(home.to_str().unwrap(), &clients) {
            assert_eq!(lane.len(), 2, "{lane:?}");
            assert_eq!(lane[0].0, "pi");
            assert_eq!(lane[0].1, "pi_ses");
            assert_eq!(lane[1].0, "senpi");
        }
    }

    /// The old `pi` lane: every file under both roots parsed as Pi, deduped
    /// first-wins on Pi's keys. Returns the token total it reported.
    fn old_pi_lane_total(files: &[PathBuf]) -> i64 {
        let mut seen = std::collections::HashSet::new();
        files
            .iter()
            .flat_map(|path| sessions::pi::parse_pi_file(path))
            .filter(|message| {
                message
                    .dedup_key
                    .as_ref()
                    .is_none_or(|key| seen.insert(key.clone()))
            })
            .map(|message| message.tokens.total())
            .sum()
    }

    #[test]
    #[serial_test::serial]
    fn test_omp_root_moves_from_pi_to_omp_with_the_same_totals() {
        // Before this client existed, the `pi` lane also scanned
        // `~/.omp/agent/sessions`. Those files now belong to `omp`, parsed by
        // the same parser: when the two trees share no records, attribution
        // moves and the total stays.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path();
        let omp_dir = home.join(".omp/agent/sessions/--p--");
        let pi_dir = home.join(".pi/agent/sessions/--p--");
        write_pi_format_session(&omp_dir, "a.jsonl", "omp_ses", "m1");
        // A fork copy in a new session file counts once.
        write_pi_format_session(&omp_dir, "fork.jsonl", "omp_fork", "m1");
        write_pi_format_session(&pi_dir, "a.jsonl", "pi_ses", "m2");
        let old_total = old_pi_lane_total(&[
            pi_dir.join("a.jsonl"),
            omp_dir.join("a.jsonl"),
            omp_dir.join("fork.jsonl"),
        ]);

        let clients = ["pi".to_string(), "omp".to_string()];
        let lanes = client_sessions_per_lane(home.to_str().unwrap(), &clients);
        for lane in &lanes {
            assert_eq!(lane.len(), 2, "{lane:?}");
            assert_eq!(lane[0].0, "omp");
            assert_eq!(lane[1], ("pi".to_string(), "pi_ses".to_string()));
        }
        let new_total: i64 =
            parse_all_messages_with_pricing(home.to_str().unwrap(), &clients, None)
                .iter()
                .map(|m| m.tokens.total())
                .sum();
        assert_eq!(new_total, old_total);
    }

    #[test]
    #[serial_test::serial]
    fn test_a_record_in_both_the_pi_and_omp_roots_now_counts_in_each() {
        // Deliberate, as upstream: there is no cross-client dedup. The old
        // single `pi` lane collapsed a record present in both trees (for
        // example a session copied from Pi into Oh My Pi); now each client
        // counts its own copy.
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = opencode_test_env(cache_home.path(), source_home.path());
        let home = source_home.path();
        let omp_dir = home.join(".omp/agent/sessions/--p--");
        let pi_dir = home.join(".pi/agent/sessions/--p--");
        write_pi_format_session(&pi_dir, "a.jsonl", "shared_ses", "m1");
        write_pi_format_session(&omp_dir, "a.jsonl", "shared_ses", "m1");
        let old_total = old_pi_lane_total(&[pi_dir.join("a.jsonl"), omp_dir.join("a.jsonl")]);

        let clients = ["pi".to_string(), "omp".to_string()];
        for lane in client_sessions_per_lane(home.to_str().unwrap(), &clients) {
            assert_eq!(
                lane,
                vec![
                    ("omp".to_string(), "shared_ses".to_string()),
                    ("pi".to_string(), "shared_ses".to_string()),
                ]
            );
        }
        let new_total: i64 =
            parse_all_messages_with_pricing(home.to_str().unwrap(), &clients, None)
                .iter()
                .map(|m| m.tokens.total())
                .sum();
        assert_eq!(new_total, 2 * old_total);
    }

    /// Lazy cache namespaces (M1108-B): a home with Claude, Codex and Pi
    /// sources, so a warm streaming scan crosses a never-released namespace,
    /// the two-stage Codex lane and a `simple_lane!` with two files.
    fn write_lazy_cache_fixture(source_home: &Path) -> (PathBuf, PathBuf) {
        let claude_dir = source_home.join(".claude/projects/lazy");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let transcript = claude_dir.join("session-lazy.jsonl");
        std::fs::write(
            &transcript,
            format!("{LAZY_CLAUDE_TURN_A}\n{LAZY_CLAUDE_TURN_B}\n"),
        )
        .unwrap();
        let codex_dir = scanner_fixture_path(source_home, ".codex/sessions");
        let codex_one = write_codex_session(
            &codex_dir,
            "a.jsonl",
            &codex_plain_fixture("lazy-codex-1", "gpt-5.4", "2026-01-01T00:00:00Z", 10, 5),
        );
        write_codex_session(
            &codex_dir,
            "b.jsonl",
            &codex_plain_fixture("lazy-codex-2", "gpt-5.4", "2026-01-02T00:00:00Z", 20, 7),
        );
        let pi_dir = source_home.join(".pi/agent/sessions/--p--");
        write_pi_format_session(&pi_dir, "a.jsonl", "lazy-pi-1", "m1");
        write_pi_format_session(&pi_dir, "b.jsonl", "lazy-pi-2", "m2");
        (transcript, codex_one)
    }

    const LAZY_CLAUDE_TURN_A: &str = r#"{"type":"assistant","sessionId":"session-lazy","timestamp":"2024-12-01T10:00:00.000Z","requestId":"req_lazy_a","message":{"id":"msg_lazy_a","model":"claude-3-5-sonnet","usage":{"input_tokens":100,"output_tokens":50}}}"#;
    const LAZY_CLAUDE_TURN_B: &str = r#"{"type":"assistant","sessionId":"session-lazy","timestamp":"2024-12-01T10:05:00.000Z","requestId":"req_lazy_b","message":{"id":"msg_lazy_b","model":"claude-3-5-sonnet","usage":{"input_tokens":200,"output_tokens":60}}}"#;

    /// (client, session, total tokens) of every streamed message, sorted.
    fn streamed_rows(source_home: &Path, clients: &[String]) -> Vec<(String, String, i64)> {
        let mut rows = Vec::new();
        scan_messages_streaming(
            source_home.to_str().unwrap(),
            clients,
            None,
            false,
            &scanner::ScannerSettings::default(),
            &|_| true,
            &mut |m| rows.push((m.client.clone(), m.session_id.clone(), m.tokens.total())),
        );
        rows.sort();
        rows
    }

    /// Canonical-store generations, independent of scope-publication
    /// bookkeeping: the durable store lives at `<usage>/...`, reached via the
    /// captured usage root so `TOKSCALE_CONFIG_DIR` profiles stay hermetic.
    fn store_source_snapshot(usage_home: &Path) -> Vec<(String, Vec<u8>, Vec<u8>, i64)> {
        let database = rusqlite::Connection::open_with_flags(
            usage_home.join("usage/source-message-cache.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = database.prepare(
            "SELECT namespace,scope,path,generation FROM mc_sources WHERE active=1 ORDER BY namespace,scope,path",
        ).unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn lazy_cache_env(cache_home: &Path) -> EnvGuard {
        EnvGuard::set(&[
            ("HOME", cache_home.as_os_str()),
            ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
            // Hermetic durable root: `SYRTIS_USAGE_DIR` outranks
            // `TOKSCALE_CONFIG_DIR`, so pin it; an outer override would
            // reroute the store away from `cache_home/usage`.
            ("SYRTIS_USAGE_DIR", cache_home.join("usage").as_os_str()),
        ])
    }

    #[test]
    #[serial_test::serial]
    fn test_streaming_warm_scan_serves_hits_without_rewriting_the_cache() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        write_lazy_cache_fixture(source_home.path());

        let cold = streamed_rows(source_home.path(), &[]);
        for client in ["claude", "codex", "pi"] {
            assert_eq!(
                cold.iter().filter(|row| row.0 == client).count(),
                2,
                "{client}: {cold:?}"
            );
        }
        let before = store_source_snapshot(cache_home.path());
        assert!(!before.is_empty());

        // Warm synchronization may update scope headers, never source generations.
        let warm = streamed_rows(source_home.path(), &[]);
        assert_eq!(warm, cold);
        assert_eq!(store_source_snapshot(cache_home.path()), before);

        let [materialized, streamed, counted] =
            client_sessions_per_lane(source_home.path().to_str().unwrap(), &[]);
        assert_eq!(materialized, streamed);
        assert_eq!(counted, streamed);
    }

    #[test]
    #[serial_test::serial]
    fn test_streaming_scan_retains_a_deleted_source_as_durable_history() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let (_, codex_one) = write_lazy_cache_fixture(source_home.path());
        let codex = message_cache::CacheIdentity::for_client(ClientId::Codex);

        streamed_rows(source_home.path(), &[]);
        assert!(message_cache::SourceMessageCache::load()
            .get(codex, &codex_one)
            .is_some());
        std::fs::remove_file(&codex_one).unwrap();
        // Durable history: the deleted source's facts persist in reports;
        // discovery unlinking never erases canonical rows.
        let after = streamed_rows(source_home.path(), &[]);
        let codex_rows: Vec<_> = after.iter().filter(|row| row.0 == "codex").collect();
        assert_eq!(
            codex_rows.len(),
            2,
            "deleted codex facts persist: {after:?}"
        );
        let reloaded = message_cache::SourceMessageCache::load();
        assert!(reloaded.get(codex, &codex_one).is_some());
        // Control: the surviving Codex file keeps its entry.
        assert!(reloaded
            .get(codex, &codex_one.with_file_name("b.jsonl"))
            .is_some());
    }

    #[test]
    #[serial_test::serial]
    fn test_streaming_claude_miss_still_sees_retained_history_with_lazy_namespaces() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        let (transcript, _) = write_lazy_cache_fixture(source_home.path());
        streamed_rows(source_home.path(), &[]);

        // A compacting rewrite drops turn A from the live file.
        std::fs::write(&transcript, format!("{LAZY_CLAUDE_TURN_B}\n")).unwrap();
        let after = streamed_rows(source_home.path(), &[]);
        let claude: Vec<i64> = after
            .iter()
            .filter(|row| row.0 == "claude")
            .map(|row| row.2)
            .collect();
        assert_eq!(claude.len(), 2, "turn A must be retained: {after:?}");
        assert_eq!(claude.iter().sum::<i64>(), 150 + 260);
    }

    #[test]
    #[serial_test::serial]
    fn test_public_snapshot_preparation_survives_nested_sync_and_eviction() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        write_lazy_cache_fixture(source_home.path());
        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let options = LocalParseOptions {
            clients: Some(vec!["claude".into(), "codex".into(), "pi".into()]),
            ..Default::default()
        };
        let snapshot = super::synchronize_local_usage(
            &context,
            options,
            super::LocalUsageSelection::LocalClients,
        )
        .unwrap();
        let mut messages = Vec::new();
        super::visit_local_usage_snapshot(
            &snapshot,
            super::LocalUsageQuery::default(),
            &|_| true,
            &mut |message| {
                if messages.is_empty() {
                    for index in 0..33 {
                        super::synchronize_local_usage(
                            &context,
                            LocalParseOptions {
                                clients: Some(vec!["claude".into(), format!("scope-{index}")]),
                                ..Default::default()
                            },
                            super::LocalUsageSelection::LocalClients,
                        )
                        .unwrap();
                    }
                }
                messages.push(message.clone());
            },
        )
        .unwrap();
        assert_eq!(
            messages.len(),
            6,
            "prepared facts survive source/scope collection"
        );
        assert!(
            super::visit_local_usage_snapshot(
                &snapshot,
                super::LocalUsageQuery::default(),
                &|_| true,
                &mut |_| {},
            )
            .is_err(),
            "an evicted snapshot must still reject a new query"
        );
    }

    /// Full durable lifecycle through production APIs: seed two Claude turns
    /// via `synchronize_local_usage` + model report (116 tokens), truncate to
    /// one turn (still 116), append a third (244), delete the source folder
    /// and rebuild from a fresh process-equivalent state (still 244), then
    /// restore plus a duplicated copy (still 244, no double count).
    #[test]
    #[serial_test::serial]
    fn test_durable_usage_survives_deletion_restart_and_copy_via_reports() {
        let usage_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", usage_home.path().as_os_str()),
            ("TOKSCALE_CONFIG_DIR", usage_home.path().as_os_str()),
            (
                "SYRTIS_USAGE_DIR",
                usage_home.path().join("usage").as_os_str(),
            ),
            ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
        ]);
        let project = source_home.path().join(".claude/projects/proof");
        std::fs::create_dir_all(&project).unwrap();
        let transcript = project.join("session.jsonl");
        // input+output+read+write per turn: a=10+20+5+3=38, b=50+20+5+3=78.
        let turn_a = r#"{"type":"assistant","timestamp":"2026-10-08T00:00:00Z","requestId":"persist-a","message":{"id":"persist-a","model":"claude-sonnet-4","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}"#;
        let turn_b = r#"{"type":"assistant","timestamp":"2026-10-08T00:01:00Z","requestId":"persist-b","message":{"id":"persist-b","model":"claude-sonnet-4","usage":{"input_tokens":50,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}"#;
        let turn_c = r#"{"type":"assistant","timestamp":"2026-10-08T00:02:00Z","requestId":"persist-c","message":{"id":"persist-c","model":"claude-sonnet-4","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}}}"#;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let totals = || {
            let options = ReportOptions {
                home_dir: Some(source_home.path().to_str().unwrap().to_string()),
                use_env_roots: false,
                clients: Some(vec!["claude".to_string()]),
                ..Default::default()
            };
            let models = runtime.block_on(super::get_model_report(options)).unwrap();
            (
                models.total_input
                    + models.total_output
                    + models.total_cache_read
                    + models.total_cache_write,
                models.total_messages,
            )
        };
        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n")).unwrap();
        assert_eq!(totals(), (116, 2));
        // Compaction removes one collected turn; usage must persist.
        std::fs::write(&transcript, format!("{turn_b}\n")).unwrap();
        assert_eq!(totals(), (116, 2));
        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n{turn_c}\n")).unwrap();
        assert_eq!(totals(), (244, 3));
        // Folder deletion + fresh scope: durable facts rebuild without logs.
        std::fs::remove_dir_all(source_home.path().join(".claude")).unwrap();
        assert_eq!(totals(), (244, 3));
        // Restore plus a duplicated copy: same stable IDs must not double.
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(&transcript, format!("{turn_a}\n{turn_b}\n{turn_c}\n")).unwrap();
        std::fs::write(
            project.join("copied.jsonl"),
            format!("{turn_a}\n{turn_b}\n{turn_c}\n"),
        )
        .unwrap();
        assert_eq!(totals(), (244, 3));
    }

    fn direct_fold_report_options(cache_home: &Path) -> (ReportOptions, Vec<String>) {
        let clients = vec!["claude".into(), "codex".into(), "pi".into()];
        let options = ReportOptions {
            home_dir: Some(cache_home.to_string_lossy().into_owned()),
            use_env_roots: false,
            clients: Some(clients.clone()),
            ..Default::default()
        };
        (options, clients)
    }

    /// Trusted direct folds must equal the public buffered visitor over mixed
    /// lanes: same client scope, same compact selection, exact payloads.
    #[test]
    #[serial_test::serial]
    fn test_direct_fold_matches_buffered_visitor_for_all_reports() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        write_lazy_cache_fixture(source_home.path());
        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let (options, _) = direct_fold_report_options(source_home.path());
        let snapshot = super::synchronize_local_usage(
            &context,
            super::local_report_scan_options(&options),
            super::LocalUsageSelection::Report,
        )
        .unwrap();

        let mut buffered = Vec::new();
        super::visit_priced_local_usage_snapshot(
            &snapshot,
            None,
            super::LocalUsageQuery::compact(),
            &|_| true,
            &mut |message| buffered.push(message.clone()),
        )
        .unwrap();
        let mut direct = Vec::new();
        super::fold_usage_snapshot_direct(&snapshot, None, &|_| true, &mut |message| {
            direct.push(message.clone())
        })
        .map(|_| ())
        .unwrap();
        assert_eq!(
            direct.len(),
            buffered.len(),
            "direct fold must emit the same selected stream"
        );
        assert!(
            direct.iter().zip(buffered.iter()).all(|(a, b)| a == b),
            "direct fold must preserve traversal order message-for-message"
        );

        // Exact-scope Report snapshot: no LocalClients index fallback (the
        // `validate_report_snapshot` exact-client check in each `*_from_snapshot`
        // constructor rejects a mismatched scope before any fold runs).
        let model =
            super::get_model_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let agents =
            super::get_agents_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let hourly =
            super::get_hourly_report_from_snapshot(&snapshot, options.clone(), None).unwrap();
        let graph = super::generate_local_graph_from_snapshot(
            &snapshot,
            options.clone(),
            None,
            super::GraphPricingMode::LocalOnly,
        )
        .unwrap();
        // Fixture carries claude/codex/pi lanes; exact token math belongs to
        // the report constructors (covered elsewhere), not this fold driver.
        assert!(!model.entries.is_empty());
        assert!(!hourly.entries.is_empty());
        assert!(!graph.graph().contributions.is_empty());
        assert_eq!(agents.total_messages, model.total_messages);
        assert_eq!(
            model.total_messages,
            buffered
                .iter()
                .map(|m| m.message_count.max(0))
                .fold(0_i32, i32::saturating_add),
            "report message total must match the selected stream"
        );
    }

    /// A malformed later row must fail the whole fold with no partial public
    /// output: the buffered visitor errors before any callback runs.
    #[test]
    #[serial_test::serial]
    fn test_malformed_later_row_publishes_no_partial_output() {
        let cache_home = tempfile::TempDir::new().unwrap();
        let source_home = tempfile::TempDir::new().unwrap();
        let _env = lazy_cache_env(cache_home.path());
        write_lazy_cache_fixture(source_home.path());
        let context = ResolvedLocalSourceContext::capture(
            Some(source_home.path().to_path_buf()),
            false,
            scanner::ScannerSettings::default(),
        )
        .unwrap();
        let (options, _) = direct_fold_report_options(source_home.path());
        let snapshot = super::synchronize_local_usage(
            &context,
            super::local_report_scan_options(&options),
            super::LocalUsageSelection::Report,
        )
        .unwrap();
        // Corrupt one cached row payload AFTER ingestion: whichever fold hits
        // it must fail whole, before any caller-visible partial output. The
        // canonical txn is released before callbacks (unpin precedes the fold),
        // so this separate writer is safe; both folds re-read the row and hit
        // checksum/deserialize error on it.
        let db_path = crate::paths::get_usage_dir().join("source-message-cache.sqlite3");
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let victim: Result<(i64, i64, i64), _> = db.query_row(
            "SELECT source_id, generation, ordinal FROM mc_rows ORDER BY source_id DESC, ordinal DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        let Ok((source_id, generation, ordinal)) = victim else {
            // No cache rows (e.g. lane selection changed): healthy-fold
            // agreement below still pins direct == buffered success.
            let mut buffered = 0usize;
            let ok_buffered = super::visit_priced_local_usage_snapshot(
                &snapshot,
                None,
                super::LocalUsageQuery::compact(),
                &|_| true,
                &mut |_| buffered += 1,
            )
            .is_ok();
            let mut folded = 0usize;
            let ok_direct =
                super::fold_usage_snapshot_direct(&snapshot, None, &|_| true, &mut |_| folded += 1)
                    .is_ok();
            assert!(ok_buffered && ok_direct && buffered == folded);
            return;
        };
        db.execute(
            "UPDATE mc_rows SET message = zeroblob(4), digest = zeroblob(32) WHERE source_id = ?1 AND generation = ?2 AND ordinal = ?3",
            rusqlite::params![source_id, generation, ordinal],
        )
        .unwrap();
        drop(db);
        let mut seen = 0usize;
        let buffered = super::visit_priced_local_usage_snapshot(
            &snapshot,
            None,
            super::LocalUsageQuery::compact(),
            &|_| true,
            &mut |_| seen += 1,
        );
        assert!(buffered.is_err(), "corrupt later row must fail the fold");
        assert_eq!(seen, 0, "buffered visitor emits nothing on failure");
        let mut folded = 0usize;
        let direct =
            super::fold_usage_snapshot_direct(&snapshot, None, &|_| true, &mut |_| folded += 1);
        assert!(direct.is_err(), "direct fold must fail on the same row");
        // Partial aggregate state lives in the caller and is dropped with the
        // `Err`: nothing publishes.
    }
}
