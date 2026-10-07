//! Scheduler source lane and pure, exact-scope aggregate queries.
//! Leases retain metadata, pinned disk handles and shared pricing, never rows.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokscale_core::{LocalUsageSelection, LocalUsageSnapshot};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_SCOPES: usize = 64;
const MAX_LEASES: usize = 32;
const MAX_LEASE_BYTES: usize = 16 * 1024 * 1024;
const EXPIRED: &str = "Local usage snapshot expired; synchronize again.";
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
type LeaseEntry = (String, Arc<Lease>);
static LEASES: LazyLock<Mutex<VecDeque<LeaseEntry>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum ScopeKind {
    Report,
    Recent,
    Quota,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scope {
    id: String,
    kind: ScopeKind,
    clients: Option<Vec<String>>,
    client_id: Option<String>,
    account_key: Option<String>,
}

impl Scope {
    fn same_producer(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.clients == other.clients
            && self.account_key == other.account_key
    }
    fn metadata_bytes(&self) -> usize {
        size_of::<Self>()
            + self.id.capacity()
            + self.client_id.as_ref().map_or(0, String::capacity)
            + self.account_key.as_ref().map_or(0, String::capacity)
            + self.clients.as_ref().map_or(0, |clients| {
                clients.capacity() * size_of::<String>()
                    + clients.iter().map(String::capacity).sum::<usize>()
            })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncRequest {
    scopes: Vec<Scope>,
    previous_lease_token: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default = "yes")]
    verify_topology: bool,
    #[serde(default)]
    refresh_pricing: bool,
}
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueryRequest {
    lease_token: String,
    scope_id: String,
    kind: String,
    year: Option<String>,
    clients: Option<Vec<String>>,
    window_secs: Option<i64>,
    now_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchRoot {
    #[serde(rename = "sourceRootURL")]
    source_root_url: String,
    #[serde(rename = "watchRootURL")]
    watch_root_url: String,
    is_directory: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OwnedWriteRule {
    #[serde(rename = "parentURL")]
    parent_url: String,
    pattern: String,
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchManifest {
    roots: Vec<WatchRoot>,
    #[serde(rename = "ignoredFileURLs")]
    ignored_file_urls: Vec<String>,
    ignored_basename_rules: Vec<OwnedWriteRule>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeSnapshot<'a> {
    id: &'a str,
    source_generation: u64,
    quota_snapshot_token: Option<&'a str>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncResult<'a> {
    lease_token: &'a str,
    root_generation: u64,
    price_generation: String,
    verified_at_ms: i64,
    scopes: Vec<ScopeSnapshot<'a>>,
    watch_manifest: WatchManifest,
    changed: bool,
}

struct SourceScope {
    producer: Scope,
    snapshot: LocalUsageSnapshot,
    manifest: Arc<tokscale_core::LocalSourceWatchManifest>,
    manifest_bytes: usize,
    cutoff: i64,
}
impl SourceScope {
    fn metadata_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.producer.metadata_bytes())
            .saturating_add(self.snapshot.metadata_bytes())
            .saturating_add(self.manifest_bytes)
            .saturating_add(64)
    }
}
struct QuotaToken(String);
impl Drop for QuotaToken {
    fn drop(&mut self) {
        let _ = crate::window_usage::release(&self.0);
    }
}
struct ScopedLease {
    spec: Scope,
    source: Arc<SourceScope>,
    quota_token: Option<Arc<QuotaToken>>,
}
struct Lease {
    scopes: Vec<ScopedLease>,
    pricing: Option<Arc<tokscale_core::pricing::PricingService>>,
    price_generation: u64,
    root_generation: u64,
}
impl Lease {
    fn metadata_bytes(&self) -> usize {
        self.scopes.iter().fold(
            size_of::<Self>().saturating_add(self.scopes.capacity() * size_of::<ScopedLease>()),
            |bytes, scope| {
                bytes
                    .saturating_add(scope.spec.metadata_bytes())
                    .saturating_add(scope.source.metadata_bytes())
                    .saturating_add(
                        scope
                            .quota_token
                            .as_ref()
                            .map_or(0, |token| size_of::<QuotaToken>() + token.0.capacity() + 64),
                    )
            },
        )
    }
}

fn id() -> Result<u64, String> {
    NEXT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| "local usage lease generation exhausted".into())
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
fn normalized_clients(clients: &mut Option<Vec<String>>) {
    if let Some(values) = clients {
        values.sort();
        values.dedup();
        if values.is_empty() {
            *clients = None;
        }
    }
}
fn parse_request<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, String> {
    if json.len() > MAX_REQUEST_BYTES {
        return Err("local usage request exceeds byte limit".into());
    }
    serde_json::from_str(json).map_err(|error| format!("invalid local usage request: {error}"))
}
fn find_lease(token: &str) -> Result<Arc<Lease>, String> {
    LEASES
        .lock()
        .iter()
        .find(|(key, _)| key == token)
        .map(|(_, lease)| Arc::clone(lease))
        .filter(|lease| lease.root_generation == crate::root_generation())
        .ok_or_else(|| EXPIRED.into())
}

/// Called only after the ABI owner acquires the local source lane.
pub(crate) fn synchronize(json: &str) -> Result<crate::EncodedLocalPayload, String> {
    let mut request: SyncRequest = parse_request(json)?;
    if request.scopes.is_empty() || request.scopes.len() > MAX_SCOPES {
        return Err("local usage synchronization requires 1..64 exact scopes".into());
    }
    let mut ids = BTreeSet::new();
    for scope in &mut request.scopes {
        if scope.id.is_empty() || !ids.insert(scope.id.clone()) {
            return Err("duplicate or empty local scope id".into());
        }
        normalized_clients(&mut scope.clients);
        match scope.kind {
            ScopeKind::Quota => {
                if scope.clients.is_some() || scope.client_id.as_ref().is_none_or(String::is_empty)
                {
                    return Err("quota scope requires its client and account identity".into());
                }
                if scope.account_key.is_some() && scope.client_id.as_deref() != Some("claude") {
                    return Err("this captured account has no local usage sources".into());
                }
            }
            _ if scope.account_key.is_some() || scope.client_id.is_some() => {
                return Err("report/recent scopes cannot carry quota account identity".into());
            }
            _ => {}
        }
    }
    let root_generation = crate::root_generation();
    let previous = request
        .previous_lease_token
        .as_deref()
        .and_then(|token| find_lease(token).ok());
    let context = crate::LocalSourceContext::current();
    let default_options = context.parse_options(None, None);
    let resolved = tokscale_core::ResolvedLocalSourceContext::capture(
        context.home_dir.clone(),
        true,
        default_options.scanner_settings.clone(),
    )
    .map_err(|error| error.to_string())?;
    let pricing = if request.refresh_pricing {
        crate::RUNTIME.block_on(tokscale_core::capture_local_usage_pricing(&resolved))
    } else if let Some(previous) = &previous {
        previous.pricing.clone()
    } else {
        tokscale_core::pricing::PricingService::load_cached_for_local_source(&resolved)
            .map(Arc::new)
    };
    let same_price = previous
        .as_ref()
        .is_some_and(|previous| same_pricing(&previous.pricing, &pricing));
    let price_generation = if same_price {
        previous.as_ref().unwrap().price_generation
    } else if pricing.is_none() {
        0
    } else {
        id()?
    };
    let mut scopes: Vec<ScopedLease> = Vec::with_capacity(request.scopes.len());
    for spec in request.scopes {
        // A single synchronization visits each distinct producing scope once,
        // including primary quota accounts which share the same physical lane.
        let checked = scopes.iter().find(|scope| scope.spec.same_producer(&spec));
        let old = previous.as_ref().and_then(|previous| {
            previous
                .scopes
                .iter()
                .find(|scope| scope.spec.same_producer(&spec))
        });
        let source = if let Some(checked) = checked {
            Arc::clone(&checked.source)
        } else if !request.force
            && !request.verify_topology
            && old.is_some_and(|old| {
                // Errors must propagate rather than become matching fingerprints.
                old.source
                    .manifest
                    .check_known_sources()
                    .is_ok_and(|changed| !changed)
            })
        {
            Arc::clone(&old.unwrap().source)
        } else {
            let options = match spec.kind {
                ScopeKind::Quota => {
                    crate::window_usage::account_parse_options(&context, &spec.account_key)?
                }
                ScopeKind::Report => tokscale_core::local_report_scan_options(
                    &context.report_options(None, spec.clients.clone()),
                ),
                _ => context.parse_options(None, spec.clients.clone()),
            };
            let scope_context = tokscale_core::ResolvedLocalSourceContext::capture(
                spec.account_key
                    .as_ref()
                    .map(PathBuf::from)
                    .or_else(|| context.home_dir.clone()),
                options.use_env_roots,
                options.scanner_settings.clone(),
            )
            .map_err(|error| error.to_string())?;
            let selection = if spec.kind == ScopeKind::Report {
                LocalUsageSelection::Report
            } else {
                LocalUsageSelection::LocalClients
            };
            // Keep the physical baseline from BEFORE ingestion. Capturing it
            // afterwards could certify newer source bytes against older facts.
            let mut accepted = None;
            for _ in 0..2 {
                let manifest =
                    tokscale_core::capture_local_source_watch_manifest(&scope_context, &options)?;
                let snapshot = tokscale_core::synchronize_local_usage(
                    &scope_context,
                    options.clone(),
                    selection,
                )?;
                if !manifest.check_known_sources()? {
                    // Admission must not wait for a concurrent slow known-file
                    // verification holding the manifest's SQLite mutex. Roots
                    // are immutable; its two page caches are bounded at 64KiB.
                    let manifest_bytes = manifest.retained_bytes().saturating_add(128 * 1024);
                    accepted = Some(SourceScope {
                        producer: spec.clone(),
                        snapshot,
                        manifest: Arc::new(manifest),
                        manifest_bytes,
                        cutoff: now_ms().saturating_sub(3_600_000),
                    });
                    break;
                }
            }
            Arc::new(
                accepted.ok_or(
                    "local sources changed during synchronization; previous lease retained",
                )?,
            )
        };
        let quota_token = if spec.kind == ScopeKind::Quota {
            let reusable = old
                .filter(|old| same_price && old.source.snapshot == source.snapshot)
                .and_then(|old| old.quota_token.as_ref())
                .filter(|token| crate::window_usage::is_live(&token.0));
            if let Some(token) = reusable {
                Some(Arc::clone(token))
            } else if let Some(token) = checked.and_then(|scope| scope.quota_token.as_ref()) {
                Some(Arc::clone(token))
            } else {
                Some(Arc::new(QuotaToken(
                    crate::window_usage::register_for_scheduler(
                        &spec.account_key,
                        source.snapshot.clone(),
                        pricing.clone(),
                        root_generation,
                        price_generation,
                    )?,
                )))
            }
        } else {
            None
        };
        scopes.push(ScopedLease {
            spec,
            source,
            quota_token,
        });
    }
    let lease = Arc::new(Lease {
        scopes,
        pricing,
        price_generation,
        root_generation,
    });
    if root_generation != crate::root_generation() {
        return Err(EXPIRED.into());
    }
    let unchanged = previous.as_ref().is_some_and(|previous| {
        previous.price_generation == lease.price_generation
            && previous.scopes.len() == lease.scopes.len()
            && lease.scopes.iter().all(|scope| {
                previous.scopes.iter().any(|old| {
                    old.spec == scope.spec
                        && old.source.snapshot == scope.source.snapshot
                        && old.quota_token.as_ref().map(|token| &token.0)
                            == scope.quota_token.as_ref().map(|token| &token.0)
                })
            })
    });
    let token = if unchanged {
        request.previous_lease_token.unwrap()
    } else {
        format!("local-{:016x}", id()?)
    };
    let payload = encode_sync(&token, &lease, !unchanged)?;
    admit(&token, &lease, true)?;
    Ok(payload)
}

fn same_pricing(
    left: &Option<Arc<tokscale_core::pricing::PricingService>>,
    right: &Option<Arc<tokscale_core::pricing::PricingService>>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}
fn file_url(path: &std::path::Path) -> Result<String, String> {
    reqwest::Url::from_file_path(path)
        .map(String::from)
        .map_err(|_| "native source path cannot be represented as a file URL".into())
}
fn encode_sync(
    token: &str,
    lease: &Lease,
    changed: bool,
) -> Result<crate::EncodedLocalPayload, String> {
    let mut watch_manifest = WatchManifest::default();
    for (index, scope) in lease.scopes.iter().enumerate() {
        if lease.scopes[..index]
            .iter()
            .any(|prior| Arc::ptr_eq(&prior.source, &scope.source))
        {
            continue;
        }
        for root in scope.source.manifest.watch_roots() {
            watch_manifest.roots.push(WatchRoot {
                source_root_url: file_url(&root.source_root)?,
                watch_root_url: file_url(&root.watch_root)?,
                is_directory: root.is_directory,
            });
        }
        for path in scope.source.manifest.ignored_paths() {
            watch_manifest.ignored_file_urls.push(file_url(path)?);
        }
        for rule in scope.source.manifest.ignored_basename_rules() {
            watch_manifest.ignored_basename_rules.push(OwnedWriteRule {
                parent_url: file_url(&rule.parent)?,
                pattern: rule.pattern.clone(),
            });
        }
    }
    watch_manifest.roots.sort_by(|a, b| {
        (&a.source_root_url, &a.watch_root_url, a.is_directory).cmp(&(
            &b.source_root_url,
            &b.watch_root_url,
            b.is_directory,
        ))
    });
    watch_manifest.roots.dedup_by(|a, b| {
        a.source_root_url == b.source_root_url
            && a.watch_root_url == b.watch_root_url
            && a.is_directory == b.is_directory
    });
    watch_manifest.ignored_file_urls.sort();
    watch_manifest.ignored_file_urls.dedup();
    watch_manifest
        .ignored_basename_rules
        .sort_by(|a, b| (&a.parent_url, &a.pattern).cmp(&(&b.parent_url, &b.pattern)));
    watch_manifest
        .ignored_basename_rules
        .dedup_by(|a, b| a.parent_url == b.parent_url && a.pattern == b.pattern);
    let scopes = lease
        .scopes
        .iter()
        .map(|scope| ScopeSnapshot {
            id: &scope.spec.id,
            source_generation: scope.source.snapshot.generation(),
            quota_snapshot_token: scope.quota_token.as_ref().map(|token| token.0.as_str()),
        })
        .collect();
    crate::encode_local_payload(&SyncResult {
        lease_token: token,
        root_generation: lease.root_generation,
        price_generation: format!("price-{:016x}", lease.price_generation),
        verified_at_ms: now_ms(),
        scopes,
        watch_manifest,
        changed,
    })
}

pub(crate) fn query(json: &str) -> Result<crate::EncodedLocalPayload, String> {
    let mut request: QueryRequest = parse_request(json)?;
    normalized_clients(&mut request.clients);
    let lease = find_lease(&request.lease_token)?;
    let scope = lease
        .scopes
        .iter()
        .find(|scope| scope.spec.id == request.scope_id)
        .ok_or("local usage query has no captured scope")?;
    if request.clients.is_some() && request.clients != scope.spec.clients {
        return Err("local usage query requires a different producing client scope".into());
    }
    let year = match request.year {
        None => None,
        Some(year) if year.is_empty() => None,
        Some(year) if year.len() == 4 && year.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(year)
        }
        Some(_) => return Err("invalid local usage year".into()),
    };
    let options = tokscale_core::ReportOptions {
        year,
        clients: scope.spec.clients.clone(),
        ..Default::default()
    };
    let pricing = lease.pricing.as_deref();
    let payload = match (scope.spec.kind, request.kind.as_str()) {
        (ScopeKind::Report, "graph") => {
            crate::usage_graph::run_from_snapshot(&scope.source.snapshot, options, pricing)
        }
        (ScopeKind::Report, "models") => {
            crate::model_report::run_from_snapshot(&scope.source.snapshot, options, pricing)
        }
        (ScopeKind::Report, "hourly") => {
            crate::hourly_report::run_from_snapshot(&scope.source.snapshot, options, pricing)
        }
        (ScopeKind::Report, "agents") => {
            crate::agents_report::run_from_snapshot(&scope.source.snapshot, options, pricing)
        }
        (ScopeKind::Recent, "rate") => crate::usage_tail::rate_from_snapshot(
            &scope.source.snapshot,
            scope.source.cutoff,
            request.window_secs.unwrap_or(60),
            request.now_ms.unwrap_or_else(now_ms),
        )
        .and_then(|rate| crate::encode_local_payload(&rate)),
        (ScopeKind::Recent, "trace") => crate::usage_tail::trace_from_snapshot(
            &scope.source.snapshot,
            scope.source.cutoff,
            request.window_secs.unwrap_or(600),
            request.now_ms.unwrap_or_else(now_ms),
        )
        .and_then(|trace| crate::encode_local_payload(&trace)),
        _ => Err("local usage query kind does not match its producing scope".into()),
    }?;
    if lease.root_generation != crate::root_generation() {
        return Err(EXPIRED.into());
    }
    // Lookup-cache growth participates in admission after every priced query.
    admit(&request.lease_token, &lease, false)?;
    Ok(payload)
}

// Metadata is conservatively counted per owning lease; shared price catalogs
// are counted once. No historical source facts or JSON trees enter this registry.
fn retained_bytes(leases: &VecDeque<LeaseEntry>) -> usize {
    let mut bytes = 0usize;
    for (index, (token, lease)) in leases.iter().enumerate() {
        bytes = bytes
            .saturating_add(token.capacity())
            .saturating_add(lease.metadata_bytes());
        if let Some(pricing) = &lease.pricing {
            if !leases.iter().take(index).any(|(_, previous)| {
                previous
                    .pricing
                    .as_ref()
                    .is_some_and(|previous| Arc::ptr_eq(previous, pricing))
            }) {
                bytes = bytes.saturating_add(pricing.retained_bytes());
            }
        }
    }
    bytes
}
fn admit(token: &str, lease: &Arc<Lease>, publish: bool) -> Result<(), String> {
    if lease.metadata_bytes().saturating_add(
        lease
            .pricing
            .as_ref()
            .map_or(0, |pricing| pricing.retained_bytes()),
    ) > MAX_LEASE_BYTES
    {
        return Err("local usage metadata exceeds byte limit".into());
    }
    let mut leases = LEASES.lock();
    if lease.root_generation != crate::root_generation() {
        return Err(EXPIRED.into());
    }
    if publish {
        if let Some(entry) = leases.iter_mut().find(|(key, _)| key == token) {
            entry.1 = Arc::clone(lease);
        } else {
            leases.push_back((token.to_owned(), Arc::clone(lease)));
        }
    } else if !leases.iter().any(|(key, _)| key == token) {
        // A released in-flight query must never resurrect its owner's lease
        // or replace a newer same-token verification manifest.
        return Err(EXPIRED.into());
    }
    while leases.len() > MAX_LEASES || retained_bytes(&leases) > MAX_LEASE_BYTES {
        let victim = leases
            .iter()
            .position(|(key, _)| key != token)
            .ok_or("local usage metadata exceeds byte limit")?;
        leases.remove(victim);
    }
    Ok(())
}

#[derive(Serialize)]
struct Released {
    released: bool,
}
pub(crate) fn release(token: &str) -> Result<crate::EncodedLocalPayload, String> {
    let mut leases = LEASES.lock();
    let before = leases.len();
    leases.retain(|(key, _)| key != token);
    crate::encode_local_payload(&Released {
        released: before != leases.len(),
    })
}
pub(crate) fn invalidate() {
    LEASES.lock().clear();
}

include!("local_usage_tests.rs");
