//! Compact quota folds over one account's pinned disk generation.
//! Swift owns window policy, model admission and confirmed attribution.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use syrtis_core::{LocalUsageQuery, LocalUsageSnapshot, UnifiedMessage};

/// `None` is the primary account. An extra Claude account is keyed by its
/// `CLAUDE_CONFIG_DIR`, the same string its card and its quota curve are keyed
/// on (`AgentUsageSnapshot::account_key`).
///
/// Only Claude accounts reach this. A captured Antigravity account also has an
/// `account_key` (a 64-hex key, not a path), but Antigravity has no local logs
/// to scan, so `DashboardModel.refreshWindowUsage` drops those accounts before
/// it builds a request. A key passed anyway matches no registered root and
/// gets `NO_REGISTERED_ROOTS`.
pub(crate) type Account = Option<String>;
const CLAUDE: &str = "claude";
#[cfg(test)]
static SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static CATALOG_SCANS: AtomicUsize = AtomicUsize::new(0);
/// The scan roots the extra config directory `dir` owns, taken from the
/// registry the scanner actually walks rather than re-derived here.
///
/// Intersecting with the registry rather than rebuilding `<dir>/projects` and
/// `<dir>/transcripts` locally keeps this honest about paths the setter
/// refused: a root that is not registered is not scanned for anybody, and
/// listing it here would claim otherwise.
///
/// **Direct children only, not every descendant.** Nothing stops a user
/// configuring both `/work` and `/work/sub` as accounts — `isRejectedRoot`
/// refuses only the empty path, `/` and the home directory
/// (`ClaudeExtraRoots.swift:53`) — and a prefix test would then hand `/work`
/// the roots of `/work/sub`, folding one account's transcripts into the
/// other's allowance. That is the defect this file exists to fix, reappearing
/// on the selection side after being fixed on the exclusion side.
///
/// A parent comparison rather than a rebuilt `{projects, transcripts}` set,
/// because every registry entry is written by `ClaudeExtraRoots.expand` and is
/// a direct child of the directory it belongs to; restating that list here
/// would be a second copy of it that could drift. The coupling is to the
/// *shape* of the expansion, not to its contents.
///
/// Note the asymmetry with `primary_exclusions`, which is deliberate: the
/// exclusion wants every descendant, because anything under a configured
/// directory is not the primary's. The selection wants exactly this account's,
/// because a nested account is somebody else's.
fn registered_roots_under(dir: &str) -> Vec<PathBuf> {
    let owner = PathBuf::from(dir);
    crate::extra_scan_paths::snapshot()
        .get(CLAUDE)
        .map(|paths| {
            paths
                .iter()
                .filter(|path| path.parent() == Some(owner.as_path()))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Every path the primary scan must not read: the configured extra config
/// directories, and the scan roots registered for them.
///
/// The union is not a spare guard — each half catches an account the other
/// misses, and the two come from registries installed by separate FFI calls
/// (`tb_set_claude_config_dirs` and `tb_set_extra_scan_paths`) with separate
/// rejection rules, so neither implies the other:
///
/// - A **config directory** catches roots under it that are not registry
///   entries. `$HOME/.cc-mirror/*/variant.json` may name any absolute
///   directory, and the scanner then reads `<that dir>/projects` — including a
///   directory nested inside a configured one, which no registry entry is a
///   prefix of.
/// - A **registered root** catches an account whose directory reached the scan
///   registry but not the config-dir registry. `ClaudeExtraRoots.install`
///   makes the two calls separately and discards the first's result, so the
///   lists can disagree, and the disagreement is silent.
///
/// Excluding neither is how the extra account's transcripts land on the primary
/// card, which is what issue #258 reports.
fn primary_exclusions() -> Vec<PathBuf> {
    let mut excluded: Vec<PathBuf> = crate::claude_config_dirs::snapshot()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(roots) = crate::extra_scan_paths::snapshot().get(CLAUDE) {
        excluded.extend(roots.iter().cloned());
    }
    excluded
}

/// The other accounts nested under `dir`, which `dir`'s own window must not
/// read: their configured directories, and every registered Claude root under
/// `dir` that is not one of `dir`'s own.
///
/// An extra account's window is scanned with `dir` as home, and the engine
/// adds `<dir>/.claude/projects`, `<dir>/.claude/transcripts` and the cowork
/// trees under it on its own. So an account configured at `dir/.claude` was
/// counted both here and in its own window. Only paths under `dir` are listed:
/// an unrelated account is never reached from this home, and an account that
/// contains this one (an ancestor) must not be listed, or the exclusion would
/// remove this account's own files. Compared component-wise as given; a nested
/// account registered under another spelling of its folder (a symlink, a
/// different case on a case-insensitive volume) is not recognised and stays
/// counted twice, as before, rather than hidden. Windows does the same
/// (Syrtis-Windows #203).
fn nested_exclusions(dir: &str, own: &[PathBuf]) -> Vec<PathBuf> {
    let owner = Path::new(dir);
    let nested = |path: &Path| path != owner && path.starts_with(owner);
    let mut excluded: Vec<PathBuf> = crate::claude_config_dirs::snapshot()
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| nested(path))
        .collect();
    if let Some(roots) = crate::extra_scan_paths::snapshot().get(CLAUDE) {
        excluded.extend(
            roots
                .iter()
                .filter(|root| !own.contains(root) && nested(root))
                .cloned(),
        );
    }
    // The engine canonicalises an exclusion before matching it, so one that is
    // nested only as spelled (`D/.claude` a symlink back to `D` or above)
    // would cover this account's own roots and empty its window. Such an
    // entry is dropped: counted twice beats confidently zero.
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let own: Vec<PathBuf> = own.iter().map(|root| canonical(root)).collect();
    excluded.retain(|path| {
        let path = canonical(path);
        !own.iter().any(|root| root.starts_with(&path))
    });
    excluded
}

/// Report options that read one account's transcripts and nothing else.
///
/// The primary keeps the real home and every client, and simply refuses the
/// configured extra accounts. An extra account is rooted at its own config
/// directory instead, where no built-in root resolves, so only its registered
/// roots contribute.
///
/// `use_env_roots` is false for an extra account for one narrow reason worth
/// stating exactly, because it is easy to overclaim: Claude's declared root is
/// `PathRoot::Home` and the flag does not gate it. What the flag gates is
/// `TOKSCALE_EXTRA_DIRS`, which can name `claude:` roots of its own and would
/// otherwise pour another account's files into this one's scan.
///
/// ponytail: a message present under two accounts' roots is counted for both,
/// because it is genuinely present in both and carries nothing that says which
/// account produced it. Measured at 2 messages in 10,389 on the reporting
/// machine. Upgrade path is an owner on the message, which costs a
/// `CACHE_FORMAT_VERSION` bump and a cold re-parse for every user.
///
/// One asymmetry this deliberately accepts: `UsageAttribution` lets a user
/// declare another client's usage against the Claude subscription, and those
/// rows are present in the primary's scan and absent from an extra account's.
/// A declaration names a client, a provider and a model and carries no account,
/// so there is no answer to which of two Claude accounts it belonged to;
/// counting it once, for the primary, beats today's counting it for both.
/// An extra account with no registered scan root cannot be answered for, and
/// says so instead of reporting an empty window.
///
/// The two registries are installed by separate FFI calls and
/// `ClaudeExtraRoots.install` deliberately wakes the quota pollers between them
/// — a stalled mount would otherwise hold the cards on the previous account set
/// for the whole filesystem timeout. So a just-added account is briefly present
/// in `claude_config_dirs` and absent from `extra_scan_paths`, and an account
/// whose roots the setter refused outright is in that state permanently.
///
/// Reporting zero rows there would be the same class of mistake this file
/// exists to fix, in the other direction: an empty scan means "nothing was
/// read", and rendering it as a window means "this account used nothing". A
/// failed read leaves the previous estimate standing, which is stale but was
/// once true; a zero is confidently wrong.
///
/// Deliberately not fixed by making the two registries atomic. They are two
/// FFI calls, so no reader-side snapshot can span them, and making the wake
/// wait for both reintroduces exactly the stall that ordering was chosen to
/// remove (`ClaudeExtraRoots.install`).
const NO_REGISTERED_ROOTS: &str =
    "This Claude account has no registered scan root yet, so its window cannot be read.";

fn account_options(
    context: &crate::LocalSourceContext,
    account: &Account,
) -> Result<syrtis_core::ReportOptions, String> {
    Ok(match account {
        None => {
            let mut options = context.report_options(None, None);
            let excluded = primary_exclusions();
            if !excluded.is_empty() {
                options
                    .scanner_settings
                    .excluded_scan_paths
                    .insert(CLAUDE.to_string(), excluded);
            }
            options
        }
        Some(dir) => {
            let roots = registered_roots_under(dir);
            if roots.is_empty() {
                return Err(NO_REGISTERED_ROOTS.to_string());
            }
            let excluded = nested_exclusions(dir, &roots);
            let mut scanner_settings = syrtis_core::scanner::ScannerSettings {
                extra_scan_paths: BTreeMap::from([(CLAUDE.to_string(), roots)]),
                ..Default::default()
            };
            if !excluded.is_empty() {
                scanner_settings
                    .excluded_scan_paths
                    .insert(CLAUDE.to_string(), excluded);
            }
            syrtis_core::ReportOptions {
                home_dir: Some(dir.clone()),
                use_env_roots: false,
                // Exactly the Claude client. Rooted at `dir`, the engine also
                // resolves every other client's home routes under it, and a
                // `.cc-mirror` variant there may name a directory anywhere;
                // those rows carry `cc-mirror/<variant>` or another client id
                // and could reach this account's card through a usage
                // attribution declaring them against Claude. The engine
                // matches this per message (`report_message_client_passes`).
                clients: Some(vec![CLAUDE.to_string()]),
                scanner_settings,
                ..Default::default()
            }
        }
    })
}

/// Capture the exact same source partition for disk synchronization.
pub(crate) fn account_parse_options(
    context: &crate::LocalSourceContext,
    account: &Account,
) -> Result<syrtis_core::LocalParseOptions, String> {
    let report = account_options(context, account)?;
    Ok(syrtis_core::LocalParseOptions {
        home_dir: report.home_dir,
        use_env_roots: report.use_env_roots,
        clients: report.clients,
        scanner_settings: report.scanner_settings,
        ..Default::default()
    })
}

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_LEASES: usize = 32;
const MAX_LEASE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_MODELS: usize = 1024;
const MAX_CATALOG_BYTES: usize = 64 * 1024;
const EXPIRED: &str = "Quota usage snapshot expired; request a new snapshot.";
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
type LeaseEntry = (String, Arc<Lease>);
static LEASES: LazyLock<Mutex<VecDeque<LeaseEntry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

struct Lease {
    account: Account,
    snapshot: LocalUsageSnapshot,
    pricing: Option<Arc<syrtis_core::pricing::PricingService>>,
    root_generation: u64,
    lease_id: u64,
    price_generation: u64,
    undated_count: u32,
    bytes: usize,
    /// Whole-history model catalog pinned to this lease's exact snapshot and
    /// account. `None` means not yet computed; `Some` (including empty) is the
    /// admitted result. Failures are never stored. Time-dependent folds stay
    /// live on every request; only this window-independent set is reused.
    catalog: Mutex<Option<Arc<Vec<String>>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    account_key: Account,
    from_ms: i64,
    until_ms: i64,
    snapshot_token: Option<String>,
    #[serde(default)]
    include_model_catalog: bool,
    #[serde(default)]
    attribution: Vec<Attribution>,
    #[serde(default)]
    model_groups: Vec<ModelGroup>,
    #[serde(default)]
    inference: Vec<InferenceJob>,
    #[serde(default)]
    windows: Vec<WindowJob>,
    diagnostics: Option<DiagnosticJobs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Attribution {
    client: String,
    provider: String,
    model: Option<String>,
    state: AttributionState,
    target: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
enum AttributionState {
    Assigned,
    Excluded,
    Unassigned,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelGroup {
    model_id: String,
    group_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InferenceJob {
    id: String,
    subscription: Option<String>,
    from_ms: i64,
    until_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WindowJob {
    id: String,
    subscription: String,
    admitted_model_ids: Option<Vec<String>>,
    current: Option<CurrentJob>,
    #[serde(default)]
    cycles: Vec<CycleJob>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CurrentJob {
    from_ms: i64,
    until_ms: i64,
    observed: Option<Span>,
    #[serde(default)]
    zones: Vec<ZoneJob>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Span {
    from_ms: i64,
    until_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ZoneJob {
    index: i64,
    lo_ms: i64,
    hi_ms: i64,
    include_lo: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CycleJob {
    reset_at_ms: i64,
    evidence_start_ms: i64,
    first_sample_ms: i64,
    last_sample_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiagnosticJobs {
    #[serde(default)]
    intervals: Vec<DiagnosticJob>,
    #[serde(default)]
    buckets: Vec<BucketJob>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiagnosticJob {
    id: String,
    from_ms: i64,
    until_ms: i64,
    include_from: bool,
    include_until: bool,
    subscription: Option<String>,
    admitted_model_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BucketJob {
    id: String,
    from_ms: i64,
    until_ms: i64,
    bucket_ms: i64,
    count: i64,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Breakdown {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    reasoning: i64,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Counter {
    message_count: i64,
    turn_count: i64,
    tokens: i64,
    tokens_ex_cache_read: i64,
    breakdown: Breakdown,
    cost: f64,
}

// Swift's per-row reporting overflow chooses MAX even for negative overflow.
fn row_sum(lanes: &[i64]) -> i64 {
    lanes
        .iter()
        .try_fold(0i64, |sum, lane| sum.checked_add(*lane))
        .unwrap_or(i64::MAX)
}

impl Breakdown {
    fn add(&mut self, row: &UnifiedMessage) {
        self.input = self.input.saturating_add(row.tokens.input);
        self.output = self.output.saturating_add(row.tokens.output);
        self.cache_read = self.cache_read.saturating_add(row.tokens.cache_read);
        self.cache_write = self.cache_write.saturating_add(row.tokens.cache_write);
        self.reasoning = self.reasoning.saturating_add(row.tokens.reasoning);
    }
}

impl Counter {
    fn add(&mut self, row: &UnifiedMessage, full: i64, fresh: i64) {
        self.message_count = self.message_count.saturating_add(1);
        self.turn_count = self.turn_count.saturating_add(i64::from(row.is_turn_start));
        self.tokens = self.tokens.saturating_add(full);
        self.tokens_ex_cache_read = self.tokens_ex_cache_read.saturating_add(fresh);
        self.breakdown.add(row);
        self.cost += row.cost;
    }
}

#[derive(Default, Serialize)]
struct Observed {
    tokens: i64,
    cost: f64,
}
impl Observed {
    fn add(&mut self, row: &UnifiedMessage, full: i64) {
        self.tokens = self.tokens.saturating_add(full);
        self.cost += row.cost;
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Coverage {
    from_ms: i64,
    until_ms: i64,
    state: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Batch {
    snapshot_token: String,
    source_generation: u64,
    root_generation: u64,
    price_generation: u64,
    model_catalog: Option<Vec<String>>,
    coverage: Coverage,
    undated_count: u32,
    processing_time_ms: u32,
    inference: Vec<InferenceResult>,
    windows: Vec<WindowResult>,
    diagnostics: Option<Diagnostics>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceResult {
    id: String,
    state: &'static str,
    first_assigned_ms: Option<i64>,
}

#[derive(Serialize)]
struct WindowResult {
    id: String,
    current: Option<CurrentResult>,
    cycles: Vec<CycleResult>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentResult {
    state: &'static str,
    unscoped_assigned_message_count: i64,
    scope_matched_nothing: bool,
    mine: Counter,
    observed: Option<Observed>,
    zones: Vec<ZoneResult>,
}

#[derive(Serialize)]
struct ZoneResult {
    index: i64,
    #[serde(flatten)]
    counter: Counter,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelResult {
    provider_id: String,
    model_id: String,
    tokens: i64,
    breakdown: Breakdown,
    cost: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CycleResult {
    reset_at_ms: i64,
    state: &'static str,
    mine_tokens: i64,
    mine_tokens_ex_cache_read: i64,
    mine_breakdown: Breakdown,
    mine_cost: f64,
    mine_message_count: i64,
    mine_turn_count: i64,
    span_tokens: i64,
    span_cost: f64,
    other_tokens: i64,
    other_cost: f64,
    other_has_assigned: bool,
    other_has_excluded: bool,
    other_has_unattributed: bool,
    models: Vec<ModelResult>,
}

struct CycleFold {
    mine: Counter,
    span: Observed,
    other: Observed,
    assigned: bool,
    excluded: bool,
    unassigned: bool,
    models: BTreeMap<String, BTreeMap<String, ModelResult>>,
}

impl CycleFold {
    fn new() -> Self {
        Self {
            mine: Counter::default(),
            span: Observed::default(),
            other: Observed::default(),
            assigned: false,
            excluded: false,
            unassigned: false,
            models: BTreeMap::new(),
        }
    }
    fn finish(self, reset_at_ms: i64) -> CycleResult {
        let mut models: Vec<_> = self
            .models
            .into_values()
            .flat_map(BTreeMap::into_values)
            .collect();
        models.sort_by(|a, b| {
            b.tokens
                .cmp(&a.tokens)
                .then_with(|| {
                    b.cost
                        .partial_cmp(&a.cost)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.provider_id.cmp(&b.provider_id))
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        CycleResult {
            reset_at_ms,
            state: "covered",
            mine_tokens: self.mine.tokens,
            mine_tokens_ex_cache_read: self.mine.tokens_ex_cache_read,
            mine_breakdown: self.mine.breakdown,
            mine_cost: self.mine.cost,
            mine_message_count: self.mine.message_count,
            mine_turn_count: self.mine.turn_count,
            span_tokens: self.span.tokens,
            span_cost: self.span.cost,
            other_tokens: self.other.tokens,
            other_cost: self.other.cost,
            other_has_assigned: self.assigned,
            other_has_excluded: self.excluded,
            other_has_unattributed: self.unassigned,
            models,
        }
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticComponents {
    input_output_reasoning: f64,
    ex_cache_read: f64,
    full: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    reasoning: f64,
    cost: f64,
    input_output_reasoning_cache_write: f64,
    incremental_input_output_reasoning: i64,
    incremental_ex_cache_read: i64,
    input_output_reasoning_tokens: i64,
}

impl DiagnosticComponents {
    fn add(&mut self, row: &UnifiedMessage, full: i64, fresh: i64) {
        let t = &row.tokens;
        let a = t.input.saturating_add(t.output).saturating_add(t.reasoning);
        self.input_output_reasoning += a as f64;
        self.input_output_reasoning_cache_write += a.saturating_add(t.cache_write) as f64;
        self.ex_cache_read += fresh as f64;
        self.full += full as f64;
        self.input += t.input as f64;
        self.output += t.output as f64;
        self.cache_read += t.cache_read as f64;
        self.cache_write += t.cache_write as f64;
        self.reasoning += t.reasoning as f64;
        self.cost += row.cost;
        self.incremental_input_output_reasoning = self
            .incremental_input_output_reasoning
            .saturating_add(t.input)
            .saturating_add(t.output)
            .saturating_add(t.reasoning);
        self.incremental_ex_cache_read = self
            .incremental_ex_cache_read
            .saturating_add(t.input)
            .saturating_add(t.output)
            .saturating_add(t.reasoning)
            .saturating_add(t.cache_write);
        self.input_output_reasoning_tokens = self.input_output_reasoning_tokens.saturating_add(a);
    }
}

#[derive(Serialize)]
struct AssignedResult {
    target: String,
    counter: Counter,
}

#[derive(Serialize)]
struct DiagnosticInterval {
    id: String,
    all: Counter,
    mine: Counter,
    assigned: Vec<AssignedResult>,
    excluded: Counter,
    unassigned: Counter,
    components: DiagnosticComponents,
}

struct DiagnosticFold {
    all: Counter,
    mine: Counter,
    assigned: BTreeMap<String, Counter>,
    excluded: Counter,
    unassigned: Counter,
    components: DiagnosticComponents,
}

impl DiagnosticFold {
    fn new() -> Self {
        Self {
            all: Counter::default(),
            mine: Counter::default(),
            assigned: BTreeMap::new(),
            excluded: Counter::default(),
            unassigned: Counter::default(),
            components: DiagnosticComponents::default(),
        }
    }
    fn finish(self, id: String) -> DiagnosticInterval {
        let mut assigned: Vec<_> = self
            .assigned
            .into_iter()
            .map(|(target, counter)| AssignedResult { target, counter })
            .collect();
        assigned.sort_by(|a, b| {
            b.counter
                .tokens
                .cmp(&a.counter.tokens)
                .then_with(|| a.target.cmp(&b.target))
        });
        DiagnosticInterval {
            id,
            all: self.all,
            mine: self.mine,
            assigned,
            excluded: self.excluded,
            unassigned: self.unassigned,
            components: self.components,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BucketResult {
    id: String,
    tokens: Vec<i64>,
    remainder_tokens: i64,
}

#[derive(Serialize)]
struct Diagnostics {
    intervals: Vec<DiagnosticInterval>,
    buckets: Vec<BucketResult>,
}

fn half_open(t: i64, from: i64, until: i64) -> bool {
    t >= from && t < until
}
fn observed(t: i64, span: &Span) -> bool {
    t > span.from_ms && t <= span.until_ms
}

fn classify<'a>(
    rules: &'a [Attribution],
    row: &UnifiedMessage,
    model: &str,
) -> (AttributionState, Option<&'a str>) {
    let source =
        |rule: &&Attribution| rule.client == row.client && rule.provider == row.provider_id;
    let rule = rules
        .iter()
        .filter(source)
        .find(|rule| rule.model.as_deref() == Some(model))
        .or_else(|| {
            rules
                .iter()
                .filter(source)
                .find(|rule| rule.model.is_none())
        });
    rule.map(|rule| (rule.state, rule.target.as_deref()))
        .unwrap_or((AttributionState::Unassigned, None))
}

impl Request {
    fn validate(&self) -> Result<(), String> {
        let range = |from: i64, until: i64, inclusive: bool| {
            if from > until
                || from < self.from_ms
                || until > self.until_ms
                || (inclusive && until == self.until_ms && from < until)
            {
                Err("quota job range is not covered by the request".to_string())
            } else {
                Ok(())
            }
        };
        if self.from_ms > self.until_ms {
            return Err("quota request range is reversed".into());
        }
        if self.attribution.len() > 128 {
            return Err("too many quota attribution records".into());
        }
        let mut keys = BTreeSet::new();
        for rule in &self.attribution {
            if !keys.insert((&rule.client, &rule.provider, &rule.model))
                || (rule.state == AttributionState::Assigned
                    && rule.target.as_deref().is_none_or(str::is_empty))
                || (rule.state != AttributionState::Assigned && rule.target.is_some())
            {
                return Err("invalid quota attribution record".into());
            }
        }
        let mut groups = BTreeSet::new();
        if self
            .model_groups
            .iter()
            .any(|group| group.group_id.is_empty() || !groups.insert(&group.model_id))
        {
            return Err("invalid quota model group mapping".into());
        }
        let mut ids = BTreeSet::new();
        for job in &self.inference {
            if !ids.insert(&job.id) {
                return Err("duplicate quota inference id".into());
            }
            range(job.from_ms, job.until_ms, false)?;
        }
        ids.clear();
        for window in &self.windows {
            if !ids.insert(&window.id) {
                return Err("duplicate quota window id".into());
            }
            if let Some(current) = &window.current {
                range(current.from_ms, current.until_ms, false)?;
                if let Some(span) = &current.observed {
                    if span.from_ms > span.until_ms {
                        return Err("quota observed range is reversed".into());
                    }
                }
                let mut indexes = BTreeSet::new();
                for zone in &current.zones {
                    if zone.lo_ms > zone.hi_ms || !indexes.insert(zone.index) {
                        return Err("invalid quota zone".into());
                    }
                }
            }
            let mut resets = BTreeSet::new();
            for cycle in &window.cycles {
                if !resets.insert(cycle.reset_at_ms) {
                    return Err("duplicate quota cycle reset".into());
                }
                range(cycle.evidence_start_ms, cycle.reset_at_ms, false)?;
                range(cycle.first_sample_ms, cycle.last_sample_ms, true)?;
            }
        }
        if let Some(jobs) = &self.diagnostics {
            ids.clear();
            for job in &jobs.intervals {
                if !ids.insert(&job.id) {
                    return Err("duplicate quota diagnostic id".into());
                }
                range(job.from_ms, job.until_ms, job.include_until)?;
            }
            ids.clear();
            let mut bucket_bytes = 0usize;
            for job in &jobs.buckets {
                if !ids.insert(&job.id) || job.bucket_ms <= 0 || !(0..=16384).contains(&job.count) {
                    return Err("invalid quota diagnostic buckets".into());
                }
                range(job.from_ms, job.until_ms, false)?;
                bucket_bytes = bucket_bytes.saturating_add(
                    (job.count as usize).saturating_mul(std::mem::size_of::<i64>()),
                );
                if bucket_bytes > MAX_LEASE_BYTES {
                    return Err("quota diagnostic buckets exceed byte limit".into());
                }
            }
        }
        Ok(())
    }
}

// The shared LocalClients visitor admits cc-mirror/* when "claude" is
// requested. Quota accounts retain the legacy report filter's literal client
// membership: mirrors may contribute to primary declarations, never extras.
fn account_client_passes(account: &Account, row: &UnifiedMessage) -> bool {
    account.is_none() || row.client == CLAUDE
}

fn capture(context: &crate::LocalSourceContext, account: &Account) -> Result<Lease, String> {
    let root_generation = crate::root_generation();
    let options = account_parse_options(context, account)?;
    let resolved = syrtis_core::ResolvedLocalSourceContext::capture(
        account
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| context.home_dir.clone()),
        options.use_env_roots,
        options.scanner_settings.clone(),
    )
    .map_err(|error| error.to_string())?;
    #[cfg(test)]
    SCAN_COUNT.fetch_add(1, Ordering::Relaxed);
    let snapshot = syrtis_core::synchronize_local_usage(
        &resolved,
        options,
        syrtis_core::LocalUsageSelection::LocalClients,
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build runtime: {error}"))?;
    let pricing = runtime.block_on(syrtis_core::capture_local_usage_pricing(&resolved));
    build_lease(account, snapshot, pricing, root_generation, None)
}

fn build_lease(
    account: &Account,
    snapshot: LocalUsageSnapshot,
    pricing: Option<Arc<syrtis_core::pricing::PricingService>>,
    root_generation: u64,
    price_generation: Option<u64>,
) -> Result<Lease, String> {
    let mut undated_count = 0u32;
    syrtis_core::visit_local_usage_snapshot(
        &snapshot,
        LocalUsageQuery {
            until_ms: Some(1),
            hydrate_opencode_aliases: false,
            ..Default::default()
        },
        &|row| account_client_passes(account, row),
        &mut |_| undated_count = undated_count.saturating_add(1),
    )?;
    if root_generation != crate::root_generation() {
        return Err(EXPIRED.into());
    }
    let lease_id = NEXT_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| "quota snapshot generation exhausted")?;
    let price_generation = price_generation.unwrap_or_else(|| {
        pricing.as_ref().map_or(0, |pricing| {
            LEASES
                .lock()
                .iter()
                .find_map(|(_, lease)| {
                    lease
                        .pricing
                        .as_ref()
                        .filter(|previous| Arc::ptr_eq(previous, pricing))
                        .map(|_| lease.price_generation)
                })
                .unwrap_or(lease_id)
        })
    });
    let bytes = snapshot.metadata_bytes()
        + account.as_ref().map_or(0, String::capacity)
        + std::mem::size_of::<Lease>()
        + 64;
    Ok(Lease {
        account: account.clone(),
        snapshot,
        pricing,
        root_generation,
        lease_id,
        price_generation,
        undated_count,
        bytes,
        catalog: Mutex::new(None),
    })
}

/// Register the scheduler's already-synchronized facts and shared price Arc.
/// No source discovery or pricing request is performed here.
pub(crate) fn register_for_scheduler(
    account: &Account,
    snapshot: LocalUsageSnapshot,
    pricing: Option<Arc<syrtis_core::pricing::PricingService>>,
    root_generation: u64,
    price_generation: u64,
) -> Result<String, String> {
    let lease = Arc::new(build_lease(
        account,
        snapshot,
        pricing,
        root_generation,
        Some(price_generation),
    )?);
    let token = format!("quota-{:016x}", lease.lease_id);
    retain_lease(&token, &lease, true)?;
    Ok(token)
}

pub(crate) fn is_live(token: &str) -> bool {
    LEASES
        .lock()
        .iter()
        .any(|(key, lease)| key == token && lease.root_generation == crate::root_generation())
}

// &Vec, not a slice: capacity is part of the retained byte count.
fn catalog_bytes(catalog: &Vec<String>) -> usize {
    catalog.iter().fold(
        std::mem::size_of::<Vec<String>>().saturating_add(
            catalog
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        ),
        |bytes, model| bytes.saturating_add(model.capacity()),
    )
}

fn retained_bytes(leases: &VecDeque<(String, Arc<Lease>)>) -> usize {
    let mut bytes = 0usize;
    for (index, (_, lease)) in leases.iter().enumerate() {
        bytes = bytes.saturating_add(lease.bytes);
        if let Some(catalog) = lease.catalog.lock().as_ref() {
            bytes = bytes.saturating_add(catalog_bytes(catalog));
        }
        // Count the captured pricing catalog once per distinct service; its
        // lazy lookup cache can grow during reads, so admission re-measures
        // after every use.
        if let Some(pricing) = &lease.pricing {
            if !leases.iter().take(index).any(|(_, prior)| {
                prior
                    .pricing
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(p, pricing))
            }) {
                bytes = bytes.saturating_add(pricing.retained_bytes());
            }
        }
    }
    bytes
}

pub(crate) fn invalidate() {
    LEASES.lock().clear();
}

pub(crate) fn release(snapshot_token: &str) -> Result<Value, String> {
    let mut leases = LEASES.lock();
    let before = leases.len();
    leases.retain(|(token, _)| token != snapshot_token);
    Ok(serde_json::json!({"released": leases.len() != before}))
}

/// Only a new capture enters the source lane; pinned quota queries stay pure.
pub(crate) fn run(request_json: &str) -> Result<crate::EncodedLocalPayload, String> {
    run_with_context(None, request_json)
}

fn run_with_context(
    context: Option<&crate::LocalSourceContext>,
    request_json: &str,
) -> Result<crate::EncodedLocalPayload, String> {
    let started = Instant::now();
    if request_json.len() > MAX_REQUEST_BYTES {
        return Err("quota request exceeds byte limit".into());
    }
    let request: Request = serde_json::from_str(request_json)
        .map_err(|error| format!("invalid quota request: {error}"))?;
    request.validate()?;
    let (token, lease, is_new) = if let Some(token) = &request.snapshot_token {
        let leases = LEASES.lock();
        let lease = leases
            .iter()
            .find(|(key, _)| key == token)
            .map(|(_, lease)| Arc::clone(lease))
            .ok_or(EXPIRED)?;
        if lease.account != request.account_key || lease.root_generation != crate::root_generation()
        {
            return Err(EXPIRED.into());
        }
        (token.clone(), lease, false)
    } else {
        let _local_usage = crate::local_usage_guard();
        let current;
        let context = match context {
            Some(context) => context,
            None => {
                current = crate::LocalSourceContext::current();
                &current
            }
        };
        let lease = Arc::new(capture(context, &request.account_key)?);
        let token = format!("quota-{:016x}", lease.lease_id);
        (token, lease, true)
    };
    let mut batch = fold(&request, &lease, token.clone())?;
    batch.processing_time_ms = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
    let value = crate::encode_local_payload(&batch)?;
    retain_lease(&token, &lease, is_new)?;
    Ok(value)
}

fn retain_lease(token: &str, lease: &Arc<Lease>, is_new: bool) -> Result<(), String> {
    let mut leases = LEASES.lock();
    if lease.root_generation != crate::root_generation() {
        return Err(EXPIRED.into());
    }
    if lease
        .bytes
        .saturating_add(lease.pricing.as_ref().map_or(0, |p| p.retained_bytes()))
        > MAX_LEASE_BYTES
    {
        leases.retain(|(key, _)| key != token);
        while retained_bytes(&leases) > MAX_LEASE_BYTES {
            leases.pop_front();
        }
        return Err("quota snapshot metadata exceeds byte limit".into());
    }
    if is_new {
        leases.push_back((token.to_owned(), Arc::clone(lease)));
    } else if !leases.iter().any(|(key, _)| key == token) {
        return Err(EXPIRED.into());
    }
    // The model catalog is an optional cache on top of an admissible lease: the
    // pricing lookup cache can grow after admission, and a previously fitting
    // catalog can then tip an otherwise admissible lease over the registry
    // bound. Drop this lease's catalog first and re-measure rather than
    // clearing the lease or failing a good query. Base/pricing hard limits
    // above are unchanged. Lock order is LEASES then catalog; fold never takes
    // LEASES, so no inversion, and no provider/source callback runs here.
    if retained_bytes(&leases) > MAX_LEASE_BYTES && lease.catalog.lock().is_some() {
        *lease.catalog.lock() = None;
    }
    while leases.len() > MAX_LEASES || retained_bytes(&leases) > MAX_LEASE_BYTES {
        if leases.len() == 1 {
            // Base/pricing alone still exceeds the bound: the catalog drop above
            // already excluded the optional cache as the cause.
            leases.clear();
            return Err("quota snapshot metadata exceeds byte limit".into());
        }
        // Preserve this caller; only older independent leases can be evicted.
        let victim = leases
            .iter()
            .position(|(key, _)| key != token)
            .ok_or(EXPIRED)?;
        leases.remove(victim);
    }
    Ok(())
}

fn fold(request: &Request, lease: &Lease, token: String) -> Result<Batch, String> {
    let scopes: Vec<Option<BTreeSet<&str>>> = request
        .windows
        .iter()
        .map(|w| {
            w.admitted_model_ids
                .as_ref()
                .map(|ids| ids.iter().map(String::as_str).collect())
        })
        .collect();
    let diagnostic_scopes: Vec<Option<BTreeSet<&str>>> = request
        .diagnostics
        .as_ref()
        .map(|d| {
            d.intervals
                .iter()
                .map(|job| {
                    job.admitted_model_ids
                        .as_ref()
                        .map(|ids| ids.iter().map(String::as_str).collect())
                })
                .collect()
        })
        .unwrap_or_default();
    let groups: BTreeMap<&str, &str> = request
        .model_groups
        .iter()
        .map(|g| (g.model_id.as_str(), g.group_id.as_str()))
        .collect();
    let mut inference: Vec<_> = request
        .inference
        .iter()
        .map(|job| InferenceResult {
            id: job.id.clone(),
            state: "covered",
            first_assigned_ms: None,
        })
        .collect();
    let mut currents: Vec<_> = request
        .windows
        .iter()
        .map(|window| {
            window.current.as_ref().map(|job| CurrentResult {
                state: "covered",
                unscoped_assigned_message_count: 0,
                scope_matched_nothing: false,
                mine: Counter::default(),
                observed: job.observed.as_ref().map(|_| Observed::default()),
                zones: job
                    .zones
                    .iter()
                    .map(|z| ZoneResult {
                        index: z.index,
                        counter: Counter::default(),
                    })
                    .collect(),
            })
        })
        .collect();
    let mut cycles: Vec<Vec<_>> = request
        .windows
        .iter()
        .map(|window| window.cycles.iter().map(|_| CycleFold::new()).collect())
        .collect();
    let mut diagnostic_folds: Vec<_> = request
        .diagnostics
        .as_ref()
        .map(|jobs| {
            jobs.intervals
                .iter()
                .map(|_| DiagnosticFold::new())
                .collect()
        })
        .unwrap_or_default();
    let mut buckets: Vec<_> = request
        .diagnostics
        .as_ref()
        .map(|jobs| {
            jobs.buckets
                .iter()
                .map(|job| BucketResult {
                    id: job.id.clone(),
                    tokens: vec![0; job.count as usize],
                    remainder_tokens: 0,
                })
                .collect()
        })
        .unwrap_or_default();
    syrtis_core::visit_priced_local_usage_snapshot(
        &lease.snapshot,
        lease.pricing.as_deref(),
        LocalUsageQuery {
            from_ms: Some(request.from_ms.max(1)),
            until_ms: Some(request.until_ms.max(1)),
            chronological: true,
            hydrate_opencode_aliases: false,
        },
        &|row| account_client_passes(&lease.account, row),
        &mut |row| {
            let model = syrtis_core::canonical_model_id(&row.model_id);
            let (state, target) = classify(&request.attribution, row, &model);
            let t = &row.tokens;
            let full = row_sum(&[t.input, t.output, t.cache_read, t.cache_write, t.reasoning]);
            let fresh = row_sum(&[t.input, t.output, t.cache_write, t.reasoning]);
            for (job, result) in request.inference.iter().zip(&mut inference) {
                if result.first_assigned_ms.is_none()
                    && half_open(row.timestamp, job.from_ms, job.until_ms)
                    && (job.subscription.is_none()
                        || (state == AttributionState::Assigned
                            && target == job.subscription.as_deref()))
                {
                    result.first_assigned_ms = Some(row.timestamp);
                }
            }
            for (index, window) in request.windows.iter().enumerate() {
                let mine = state == AttributionState::Assigned
                    && target == Some(window.subscription.as_str());
                let admitted = scopes[index]
                    .as_ref()
                    .is_none_or(|ids| ids.contains(model.as_str()));
                if let (Some(job), Some(result)) = (&window.current, &mut currents[index]) {
                    if mine && half_open(row.timestamp, job.from_ms, job.until_ms) {
                        result.unscoped_assigned_message_count =
                            result.unscoped_assigned_message_count.saturating_add(1);
                        if admitted {
                            result.mine.add(row, full, fresh);
                            if let (Some(span), Some(value)) = (&job.observed, &mut result.observed)
                            {
                                if observed(row.timestamp, span) {
                                    value.add(row, full);
                                }
                            }
                            for (zone, value) in job.zones.iter().zip(&mut result.zones) {
                                if (row.timestamp > zone.lo_ms
                                    || (zone.include_lo && row.timestamp == zone.lo_ms))
                                    && row.timestamp <= zone.hi_ms
                                {
                                    value.counter.add(row, full, fresh);
                                }
                            }
                        }
                    }
                }
                if !admitted {
                    continue;
                }
                for (job, result) in window.cycles.iter().zip(&mut cycles[index]) {
                    if mine
                        && row.timestamp > job.first_sample_ms
                        && row.timestamp <= job.last_sample_ms
                    {
                        result.span.add(row, full);
                    }
                    if !half_open(row.timestamp, job.evidence_start_ms, job.reset_at_ms) {
                        continue;
                    }
                    if mine {
                        result.mine.add(row, full, fresh);
                        let group = groups
                            .get(model.as_str())
                            .copied()
                            .unwrap_or(model.as_str());
                        if !result.models.contains_key(row.provider_id.as_str()) {
                            result
                                .models
                                .insert(row.provider_id.clone(), BTreeMap::new());
                        }
                        let models = result
                            .models
                            .get_mut(row.provider_id.as_str())
                            .expect("inserted provider");
                        if !models.contains_key(group) {
                            models.insert(
                                group.to_string(),
                                ModelResult {
                                    provider_id: row.provider_id.clone(),
                                    model_id: group.to_string(),
                                    ..Default::default()
                                },
                            );
                        }
                        let entry = models.get_mut(group).expect("inserted model");
                        entry.tokens = entry.tokens.saturating_add(full);
                        entry.breakdown.add(row);
                        entry.cost += row.cost;
                    } else {
                        result.other.add(row, full);
                        match state {
                            AttributionState::Assigned => result.assigned = true,
                            AttributionState::Excluded => result.excluded = true,
                            AttributionState::Unassigned => result.unassigned = true,
                        }
                    }
                }
            }
            if let Some(jobs) = &request.diagnostics {
                for (index, (job, result)) in
                    jobs.intervals.iter().zip(&mut diagnostic_folds).enumerate()
                {
                    if (row.timestamp < job.from_ms
                        || (!job.include_from && row.timestamp == job.from_ms))
                        || (row.timestamp > job.until_ms
                            || (!job.include_until && row.timestamp == job.until_ms))
                        || !diagnostic_scopes[index]
                            .as_ref()
                            .is_none_or(|ids| ids.contains(model.as_str()))
                    {
                        continue;
                    }
                    result.all.add(row, full, fresh);
                    match state {
                        AttributionState::Assigned => {
                            if let Some(target) = target {
                                if !result.assigned.contains_key(target) {
                                    result
                                        .assigned
                                        .insert(target.to_string(), Counter::default());
                                }
                                result
                                    .assigned
                                    .get_mut(target)
                                    .expect("inserted target")
                                    .add(row, full, fresh);
                            }
                        }
                        AttributionState::Excluded => result.excluded.add(row, full, fresh),
                        AttributionState::Unassigned => result.unassigned.add(row, full, fresh),
                    }
                    if job.subscription.is_none()
                        || (state == AttributionState::Assigned
                            && target == job.subscription.as_deref())
                    {
                        result.mine.add(row, full, fresh);
                        result.components.add(row, full, fresh);
                    }
                }
                for (job, result) in jobs.buckets.iter().zip(&mut buckets) {
                    if !half_open(row.timestamp, job.from_ms, job.until_ms) {
                        continue;
                    }
                    let index = (i128::from(row.timestamp) - i128::from(job.from_ms))
                        / i128::from(job.bucket_ms);
                    if index < i128::from(job.count) {
                        result.tokens[index as usize] =
                            result.tokens[index as usize].saturating_add(full);
                    } else {
                        result.remainder_tokens = result.remainder_tokens.saturating_add(full);
                    }
                }
            }
        },
    )?;
    let model_catalog = if request.include_model_catalog {
        let cached = lease.catalog.lock().clone();
        if let Some(cached) = cached {
            Some(cached.as_ref().clone())
        } else {
            // Whole-history traversal, independent of moving windows, scopes and
            // prices, so one admission per lease serves every later request on
            // the same token. Time-dependent folds above stay live every time.
            // ponytail: oversized catalogs answer complete but stay uncached, so a
            // huge model set traverses again next time instead of pinning RAM.
            let mut models = BTreeSet::new();
            syrtis_core::visit_local_usage_snapshot(
                &lease.snapshot,
                LocalUsageQuery {
                    hydrate_opencode_aliases: false,
                    ..Default::default()
                },
                &|row| account_client_passes(&lease.account, row),
                &mut |row| {
                    models.insert(syrtis_core::canonical_model_id(&row.model_id));
                },
            )?;
            let catalog: Vec<String> = models.into_iter().collect();
            // Empty is a result, not a miss: an account with no rows answers
            // once. Failures return above and are never stored.
            #[cfg(test)]
            CATALOG_SCANS.fetch_add(1, Ordering::Relaxed);
            if catalog.len() <= MAX_CATALOG_MODELS && catalog_bytes(&catalog) <= MAX_CATALOG_BYTES {
                // Never turn a good uncached answer into a new admission failure:
                // a near-limit lease answers complete and stays uncached.
                let admitted = lease
                    .bytes
                    .saturating_add(catalog_bytes(&catalog))
                    .saturating_add(lease.pricing.as_ref().map_or(0, |p| p.retained_bytes()));
                if admitted <= MAX_LEASE_BYTES {
                    *lease.catalog.lock() = Some(Arc::new(catalog.clone()));
                }
            }
            Some(catalog)
        }
    } else {
        None
    };
    let windows = request
        .windows
        .iter()
        .zip(currents)
        .zip(cycles)
        .map(|((job, current), cycles)| {
            let current = current.map(|mut result| {
                result.scope_matched_nothing = job.admitted_model_ids.is_some()
                    && result.mine.message_count == 0
                    && result.unscoped_assigned_message_count > 0;
                result
            });
            WindowResult {
                id: job.id.clone(),
                current,
                cycles: job
                    .cycles
                    .iter()
                    .zip(cycles)
                    .map(|(job, fold)| fold.finish(job.reset_at_ms))
                    .collect(),
            }
        })
        .collect();
    Ok(Batch {
        snapshot_token: token,
        source_generation: lease.snapshot.generation(),
        root_generation: lease.root_generation,
        price_generation: lease.price_generation,
        model_catalog,
        coverage: Coverage {
            from_ms: request.from_ms,
            until_ms: request.until_ms,
            state: "complete",
        },
        undated_count: lease.undated_count,
        processing_time_ms: 0,
        inference,
        windows,
        diagnostics: request.diagnostics.as_ref().map(|jobs| Diagnostics {
            intervals: jobs
                .intervals
                .iter()
                .zip(diagnostic_folds)
                .map(|(job, fold)| fold.finish(job.id.clone()))
                .collect(),
            buckets,
        }),
    })
}

#[cfg(test)]
#[path = "window_usage_tests.rs"]
mod aggregate_tests;
