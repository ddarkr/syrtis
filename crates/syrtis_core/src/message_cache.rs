use crate::clients::ClientId;
use crate::sessions::codex::CodexParseState;
use crate::UnifiedMessage;
use bincode::Options;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
#[cfg(test)]
use std::io::BufWriter;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

// CACHE_FORMAT_VERSION changes only when the serialized storage layout or a
// cross-client type such as UnifiedMessage changes incompatibly. Parser-only
// changes belong in parser_version() so one client cannot evict every other
// client's cached transcripts.
// 2: Related-file fingerprints now retain their paths and whether they were
// absent when cached. Claude sidechain parent candidates can therefore be
// revalidated without reparsing the sidechain on every warm scan, while a
// later-created parent transcript still invalidates the entry.
// 3: CachedSourceEntry gained `retained_keys`, a new positional field shared
// by every namespace's serialized entries — Codex, Grok, Copilot and all
// others, not just Claude. This is a storage-layout change, not a
// parser-only one, so it belongs here rather than in a single client's
// parser_version: the envelope's format_version gate must reject every
// namespace's pre-bump shard by version before any of them are decoded
// against the new 8-field layout. This invalidates every namespace's cached
// shards once.
// 4: TokenBreakdown gained `cache_write_1h`, appended to a type every
// namespace's payload embeds. Unlike 2 and 3 this bump does not discard the
// old shards: a retaining namespace's format-3 payload is decoded through
// `mod format3` and carried forward, so the turns a compacting rewrite took
// out of a Claude transcript survive it. Everything else still goes cold,
// which costs those namespaces nothing.
// 5: UnifiedMessage appends cache-split provenance. Frozen format-3/4 Claude
// payloads preserve retained-only history without inventing provenance.
const CACHE_FORMAT_VERSION: u32 = 5;
// V2 intentionally starts cold and leaves source-message-cache.bin untouched:
// the monolith did not record a trustworthy parser owner for migration.
const CACHE_SHARD_DIRNAME: &str = "source-message-cache-v2";
const CACHE_LOCK_FILENAME: &str = "source-message-cache.lock";
const CACHE_SHARD_COUNT: usize = 256;
const MAX_CACHE_SHARD_BYTES: u64 = 256 * 1024 * 1024;
const FINGERPRINT_SAMPLE_BYTES: usize = 4096;
const FINGERPRINT_SAMPLE_POINTS: usize = 5;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

#[cfg(test)]
thread_local! {
    static FULL_HASH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Default fact-store root: the app-owned durable usage dir, never the
/// disposable config cache or a runtime/temp dir. Legacy migration seeds
/// still read the old cache dirs explicitly; this default only routes
/// canonical store access (introspection, compat, and default opens).
fn cache_dir() -> Option<PathBuf> {
    Some(crate::paths::get_usage_dir())
}

#[cfg(test)]
fn cache_shard_dir() -> Option<PathBuf> {
    Some(cache_dir()?.join(CACHE_SHARD_DIRNAME))
}

#[cfg(test)]
fn ensure_cache_dir(dir: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(dir) {
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            return Err(std::io::Error::other(
                "cache directory is not a real directory",
            ));
        }
    }
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
fn warn_cache_failure_once(context: &'static str, _path: &Path, _error: &impl std::fmt::Display) {
    // Cache persistence is fail-soft. Report only the bounded category: the
    // configured cache root and native error can contain profile information,
    // and local report callers must never copy either into stderr or tracing.
    tracing::warn!(%context, "source message cache failure");

    // Most non-TUI commands (including `submit`) do not install a tracing
    // subscriber. Surface the bounded category directly once per process so a
    // permanently cold cache can never fail silently again.
    static WARNED_CONTEXTS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let warned = WARNED_CONTEXTS.get_or_init(|| Mutex::new(HashSet::new()));
    if warned.lock().is_ok_and(|mut warned| warned.insert(context)) {
        eprintln!("tokscale: warning: {context}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileSampleHash {
    pub offset: u64,
    pub len: u64,
    pub hash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SourceFingerprint {
    pub size: u64,
    pub modified_ns: u64,
    pub sample_hashes: Vec<FileSampleHash>,
    pub content_hash: [u8; 32],
    pub related_files: Vec<RelatedFileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelatedFileFingerprint {
    pub suffix: String,
    pub path: CachedPath,
    pub exists: bool,
    pub size: u64,
    pub modified_ns: u64,
    pub sample_hashes: Vec<FileSampleHash>,
    pub content_hash: [u8; 32],
}

/// Metadata siblings a Grok `updates.jsonl` session depends on.
/// Keep this list aligned with the parser fingerprint and live-tail probes.
pub(crate) const GROK_METADATA_SIBLINGS: [&str; 3] =
    ["signals.json", "summary.json", "events.jsonl"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FingerprintStatus {
    /// Size and nanosecond mtime still match for the source and every parser
    /// sidecar, and their bounded samples still match. No full-file SHA-256 was
    /// computed, so a warm scan reads at most 20 KiB per watched file.
    Unchanged,
    /// Metadata changed, so a complete fingerprint was rebuilt to distinguish
    /// a real content change from a metadata-only touch.
    Changed(SourceFingerprint),
}

impl SourceFingerprint {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        Self::from_path_with_related(path, std::iter::empty())
    }

    pub(crate) fn from_path_samples_only(path: &Path) -> Option<Self> {
        Self::from_path_with_related_mode(path, std::iter::empty(), ContentHashMode::SamplesOnly)
    }

    pub(crate) fn from_sqlite_path(path: &Path) -> Option<Self> {
        let mut fingerprint = Self::from_path_samples_only(path)?;
        fingerprint
            .related_files
            .push(sqlite_wal_fingerprint(&append_path_suffix(path, "-wal"))?);
        Some(fingerprint)
    }

    /// Fingerprint for Copilot Desktop's SQLite database, WAL, and dynamic
    /// `session-state/*/events.jsonl` dependencies. Any unreadable dependency
    /// fails open to a cache miss instead of serving a DB-only stale entry.
    pub(crate) fn from_copilot_desktop_path(path: &Path) -> Option<Self> {
        let related_paths = copilot_desktop_related_paths(path)?;
        let mut fingerprint =
            Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)?;
        fingerprint
            .related_files
            .push(sqlite_wal_fingerprint(&append_path_suffix(path, "-wal"))?);
        fingerprint
            .related_files
            .sort_by(|left, right| left.suffix.cmp(&right.suffix));
        Some(fingerprint)
    }

    #[cfg(test)]
    pub(crate) fn check_copilot_desktop_path_samples_only(
        path: &Path,
        cached: Option<&Self>,
    ) -> Option<FingerprintStatus> {
        let current = Self::from_copilot_desktop_path(path)?;
        Some(if cached == Some(&current) {
            FingerprintStatus::Unchanged
        } else {
            FingerprintStatus::Changed(current)
        })
    }

    pub(crate) fn from_jcode_path(path: &Path) -> Option<Self> {
        let related_paths = std::iter::once((
            ".journal.jsonl".to_string(),
            crate::sessions::jcode::jcode_journal_path(path),
        ));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    /// Fingerprint for a Roo-family task (`ui_messages.json`) and its sibling
    /// `api_conversation_history.json`. `parse_roo_kilo_file` reads the history
    /// sibling for the model and agent, so a history-only rewrite (the UI file
    /// unchanged) must still invalidate the cache or reports keep stale
    /// model/agent/pricing.
    pub(crate) fn from_roo_path(path: &Path) -> Option<Self> {
        let history = crate::sessions::roocode::history_path_for_ui_messages(path);
        let related_paths = std::iter::once(("api_conversation_history.json".to_string(), history));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    /// Fingerprint for a Claude Code JSONL file that may have a sibling `.meta.json`
    /// sidecar. When the sidecar appears or changes (e.g. after a Claude Code upgrade),
    /// the fingerprint changes and the cache invalidates.
    #[cfg(test)]
    pub(crate) fn from_claude_code_path_with_home(
        path: &Path,
        home_dir: Option<&Path>,
    ) -> Option<Self> {
        let mut related = Vec::new();

        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let meta_filename = format!("{}.meta.json", stem);
            related.push((".meta.json".to_string(), path.with_file_name(meta_filename)));
        }

        if let Some(variant_path) = crate::cc_mirror::variant_file_for_session_path(path, home_dir)
        {
            related.push(("cc-mirror/variant.json".to_string(), variant_path));
        }
        for (index, parent_path) in
            crate::sessions::claudecode::parent_session_paths_for_cache(path)
                .into_iter()
                .enumerate()
        {
            related.push((format!("parent-session-{index}.jsonl"), parent_path));
        }

        Self::from_path_with_related_mode(path, related, ContentHashMode::SamplesOnly)
    }

    /// Fingerprint for a Grok `updates.jsonl` session and every sibling read by
    /// its parser for rollup and session metadata.
    pub(crate) fn from_grok_path(path: &Path) -> Option<Self> {
        if path.file_name().and_then(|name| name.to_str()) == Some("unified.jsonl") {
            return Self::from_path_samples_only(path);
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let related_paths = ["signals.json", "summary.json", "events.jsonl"]
            .into_iter()
            .map(|name| (name.to_string(), parent.join(name)));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    /// Fingerprint for a Kiro source file. IDE sessions consume a sibling
    /// `messages.jsonl`, while CLI `*.json` headers consume same-stem `*.jsonl`.
    /// Global-storage and `.chat` snapshots are self-contained.
    pub(crate) fn from_kiro_path(path: &Path) -> Option<Self> {
        let Some(messages) = crate::sessions::kiro::kiro_related_messages_path(path) else {
            return Self::from_path_samples_only(path);
        };
        let related_paths = std::iter::once(("messages.jsonl".to_string(), messages));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    pub(crate) fn from_droid_path(path: &Path) -> Option<Self> {
        let Some(jsonl) = crate::sessions::droid::droid_jsonl_path(path) else {
            return Self::from_path_samples_only(path);
        };
        let related_paths = std::iter::once(("session.jsonl".to_string(), jsonl));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    pub(crate) fn from_kimi_path(path: &Path) -> Option<Self> {
        if crate::sessions::kimi::is_kimi_code_path(path) {
            return Self::from_path_samples_only(path);
        }
        let Some(config) = crate::sessions::kimi::kimi_config_path(path) else {
            return Self::from_path_samples_only(path);
        };
        let related_paths = std::iter::once(("config.json".to_string(), config));
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::SamplesOnly)
    }

    pub(crate) fn check_path(path: &Path, cached: Option<&Self>) -> Option<FingerprintStatus> {
        Self::check_path_with_related(path, std::iter::empty(), cached)
    }

    #[cfg(test)]
    pub(crate) fn check_sqlite_path(
        path: &Path,
        cached: Option<&Self>,
    ) -> Option<FingerprintStatus> {
        let current = Self::from_sqlite_path(path)?;
        Some(if cached == Some(&current) {
            FingerprintStatus::Unchanged
        } else {
            FingerprintStatus::Changed(current)
        })
    }

    pub(crate) fn check_claude_code_path_with_home_samples_only(
        path: &Path,
        cached: Option<&Self>,
        home_dir: Option<&Path>,
    ) -> Option<FingerprintStatus> {
        Self::check_claude_code_path_with_home_mode(
            path,
            cached,
            home_dir,
            ContentHashMode::SamplesOnly,
        )
    }

    fn check_claude_code_path_with_home_mode(
        path: &Path,
        cached: Option<&Self>,
        home_dir: Option<&Path>,
        mode: ContentHashMode,
    ) -> Option<FingerprintStatus> {
        let mut related = Vec::new();

        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            let meta_filename = format!("{}.meta.json", stem);
            related.push((".meta.json".to_string(), path.with_file_name(meta_filename)));
        }

        if let Some(variant_path) = crate::cc_mirror::variant_file_for_session_path(path, home_dir)
        {
            related.push(("cc-mirror/variant.json".to_string(), variant_path));
        }

        let primary_matches =
            cached.and_then(|fingerprint| primary_fingerprint_matches(path, fingerprint));
        let parent_paths = cached
            .filter(|_| primary_matches == Some(true))
            .map(cached_claude_parent_paths)
            .unwrap_or_else(|| {
                crate::sessions::claudecode::parent_session_paths_for_cache(path)
                    .into_iter()
                    .enumerate()
                    .map(|(index, parent_path)| {
                        (format!("parent-session-{index}.jsonl"), parent_path)
                    })
                    .collect()
            });
        related.extend(parent_paths);

        Self::check_path_with_related_mode_and_primary(path, related, cached, mode, primary_matches)
    }

    #[cfg(test)]
    pub(crate) fn check_kiro_path_samples_only(
        path: &Path,
        cached: Option<&Self>,
    ) -> Option<FingerprintStatus> {
        Self::check_kiro_path_with_mode(path, cached, ContentHashMode::SamplesOnly)
    }

    #[cfg(test)]
    fn check_kiro_path_with_mode(
        path: &Path,
        cached: Option<&Self>,
        mode: ContentHashMode,
    ) -> Option<FingerprintStatus> {
        let Some(messages) = crate::sessions::kiro::kiro_related_messages_path(path) else {
            return Self::check_path_with_related_mode(path, std::iter::empty(), cached, mode);
        };
        let related_paths = std::iter::once(("messages.jsonl".to_string(), messages));
        Self::check_path_with_related_mode(path, related_paths, cached, mode)
    }

    fn check_path_with_related<I>(
        path: &Path,
        related_paths: I,
        cached: Option<&Self>,
    ) -> Option<FingerprintStatus>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        Self::check_path_with_related_mode(path, related_paths, cached, ContentHashMode::Full)
    }

    fn check_path_with_related_mode<I>(
        path: &Path,
        related_paths: I,
        cached: Option<&Self>,
        mode: ContentHashMode,
    ) -> Option<FingerprintStatus>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        Self::check_path_with_related_mode_and_primary(path, related_paths, cached, mode, None)
    }

    fn check_path_with_related_mode_and_primary<I>(
        path: &Path,
        related_paths: I,
        cached: Option<&Self>,
        mode: ContentHashMode,
        primary_matches: Option<bool>,
    ) -> Option<FingerprintStatus>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        let related_paths: Vec<(String, PathBuf)> = related_paths.into_iter().collect();
        let cache_hit = cached.is_some_and(|fingerprint| {
            primary_matches
                .unwrap_or_else(|| primary_fingerprint_matches(path, fingerprint).unwrap_or(false))
                && related_fingerprint_metadata_matches(&related_paths, fingerprint)
                    .unwrap_or(false)
        });
        if cache_hit {
            return Some(FingerprintStatus::Unchanged);
        }

        Self::from_path_with_related_mode(path, related_paths, mode).map(FingerprintStatus::Changed)
    }

    fn from_path_with_related<I>(path: &Path, related_paths: I) -> Option<Self>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        Self::from_path_with_related_mode(path, related_paths, ContentHashMode::Full)
    }

    fn from_path_with_related_mode<I>(
        path: &Path,
        related_paths: I,
        mode: ContentHashMode,
    ) -> Option<Self>
    where
        I: IntoIterator<Item = (String, PathBuf)>,
    {
        let (size, modified_ns, sample_hashes, content_hash) = file_fingerprint_parts(path, mode)?;
        let mut related_files: Vec<RelatedFileFingerprint> = related_paths
            .into_iter()
            .map(|(suffix, related_path)| {
                RelatedFileFingerprint::from_path(suffix, &related_path, mode)
            })
            .collect::<Option<_>>()?;
        related_files.sort_by(|left, right| left.suffix.cmp(&right.suffix));

        Some(Self {
            size,
            modified_ns,
            sample_hashes,
            content_hash,
            related_files,
        })
    }
}

fn copilot_desktop_related_paths(path: &Path) -> Option<Vec<(String, PathBuf)>> {
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(path).ok()?;
    let events = crate::sessions::copilot_desktop::session_state_event_paths(path).ok()?;
    let mut related_paths = Vec::new();

    for event in events {
        File::open(&event).ok()?;
        let label = event
            .strip_prefix(root)
            .unwrap_or(&event)
            .to_string_lossy()
            .into_owned();
        related_paths.push((label, event));
    }
    Some(related_paths)
}

/// SQLite-only equivalence: absent and readable zero-byte WALs both contain
/// no uncheckpointed frames. Generic sibling fingerprints retain exact presence.
fn sqlite_wal_fingerprint(path: &Path) -> Option<RelatedFileFingerprint> {
    let empty = || RelatedFileFingerprint {
        suffix: "-wal".to_string(),
        path: CachedPath::from_path(path),
        exists: false,
        size: 0,
        modified_ns: 0,
        sample_hashes: Vec::new(),
        content_hash: [0; 32],
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(empty()),
        Err(_) => return None,
    };
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    if metadata.len() == 0 {
        // Opening proves permission admission even when there are no samples.
        // A byte appearing after metadata must not normalize into absence.
        let mut byte = [0; 1];
        if file.read(&mut byte).ok()? == 0 {
            return Some(empty());
        }
    }
    RelatedFileFingerprint::from_path("-wal".to_string(), path, ContentHashMode::SamplesOnly)
}

impl RelatedFileFingerprint {
    fn from_path(suffix: String, path: &Path, mode: ContentHashMode) -> Option<Self> {
        let cached_path = CachedPath::from_path(path);
        match path.metadata() {
            Ok(_) => {
                let (size, modified_ns, sample_hashes, content_hash) =
                    file_fingerprint_parts(path, mode)?;
                Some(Self {
                    suffix,
                    path: cached_path,
                    exists: true,
                    size,
                    modified_ns,
                    sample_hashes,
                    content_hash,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Self {
                suffix,
                path: cached_path,
                exists: false,
                size: 0,
                modified_ns: 0,
                sample_hashes: Vec::new(),
                content_hash: [0; 32],
            }),
            Err(_) => None,
        }
    }
}

fn cached_claude_parent_paths(cached: &SourceFingerprint) -> Vec<(String, PathBuf)> {
    cached
        .related_files
        .iter()
        .filter(|related| related.suffix.starts_with("parent-session-"))
        .map(|related| (related.suffix.clone(), related.path.to_path_buf()))
        .collect()
}

fn primary_fingerprint_matches(path: &Path, cached: &SourceFingerprint) -> Option<bool> {
    let (size, modified_ns) = metadata_signature(path).ok()?;
    if size != cached.size || modified_ns != cached.modified_ns {
        return Some(false);
    }
    Some(compute_sample_hashes(path, size)? == cached.sample_hashes)
}

fn metadata_signature(path: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = path.metadata()?;
    let modified_ns = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos() as u64;
    Ok((metadata.len(), modified_ns))
}

fn related_fingerprint_metadata_matches(
    related_paths: &[(String, PathBuf)],
    cached: &SourceFingerprint,
) -> Option<bool> {
    if cached.related_files.len() != related_paths.len() {
        return Some(false);
    }

    for (suffix, related_path) in related_paths {
        let Some(related) = cached
            .related_files
            .iter()
            .find(|related| related.suffix == *suffix)
        else {
            return Some(false);
        };
        if related.path != CachedPath::from_path(related_path) {
            return Some(false);
        }
        if !related_file_fingerprint_matches(related, related_path)? {
            return Some(false);
        }
    }

    Some(true)
}

fn related_file_fingerprint_matches(related: &RelatedFileFingerprint, path: &Path) -> Option<bool> {
    match metadata_signature(path) {
        Ok((size, modified_ns)) => Some(
            related.exists
                && related.size == size
                && related.modified_ns == modified_ns
                && compute_sample_hashes(path, size)? == related.sample_hashes,
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(!related.exists),
        Err(_) => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CodexIncrementalCache {
    pub state: CodexParseState,
    pub consumed_offset: u64,
    pub ends_with_newline: bool,
    pub prefix_hash: [u8; 32],
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(Vec<u8>);

#[cfg(unix)]
impl CachedPath {
    /// Stores the raw OS string with no normalisation, so two `Path`s that
    /// name the same file but differ byte for byte are different cache keys.
    /// On Windows that includes mixed separators: a `join("a/b")` literal and
    /// the all-backslash path a directory walk produces will not match.
    pub(crate) fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;

        Self(path.as_os_str().as_bytes().to_vec())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(self.0.clone()))
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(self.0))
    }

    fn update_digest(&self, hasher: &mut Sha256) {
        hasher.update(&self.0);
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(Vec<u16>);

#[cfg(windows)]
impl CachedPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        use std::os::windows::ffi::OsStrExt;

        Self(path.as_os_str().encode_wide().collect())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;

        PathBuf::from(OsString::from_wide(&self.0))
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        self.to_path_buf()
    }

    fn update_digest(&self, hasher: &mut Sha256) {
        for code_unit in &self.0 {
            hasher.update(code_unit.to_le_bytes());
        }
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct CachedPath(String);

#[cfg(not(any(unix, windows)))]
impl CachedPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        Self(path.to_string_lossy().into_owned())
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }

    pub(crate) fn into_path_buf(self) -> PathBuf {
        PathBuf::from(self.0)
    }

    fn update_digest(&self, hasher: &mut Sha256) {
        hasher.update(self.0.as_bytes());
    }
}

fn update_path_digest(path: &Path, hasher: &mut Sha256) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hasher.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hasher.update(path.to_string_lossy().as_bytes());
}

fn legacy_shard_index(namespace: &str, path: &CachedPath) -> usize {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update([0]);
    path.update_digest(&mut hasher);
    usize::from(hasher.finalize()[0]) % CACHE_SHARD_COUNT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CacheIdentity {
    pub(crate) namespace: &'static str,
    parser_version: u32,
    scope: [u8; 32],
}

impl CacheIdentity {
    pub(crate) fn for_client(client: ClientId) -> Self {
        Self {
            namespace: client.as_str(),
            parser_version: parser_version(client),
            scope: [0; 32],
        }
    }

    pub(crate) const fn synthetic() -> Self {
        Self {
            namespace: "synthetic",
            parser_version: 1,
            scope: [0; 32],
        }
    }

    pub(crate) fn with_scope(mut self, home: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"tokscale-parser-context-v1");
        update_path_digest(home, &mut hasher);
        self.scope = hasher.finalize().into();
        self
    }

    pub(crate) fn scope_bytes(&self) -> [u8; 32] {
        self.scope
    }

    pub(crate) fn current_for_namespace_carry(namespace: &str) -> Option<Self> {
        Self::current_for_namespace(namespace)
    }

    fn current_for_namespace(namespace: &str) -> Option<Self> {
        if namespace == "synthetic" {
            return Some(Self::synthetic());
        }
        ClientId::from_str(namespace).map(Self::for_client)
    }

    #[cfg(test)]
    fn all() -> impl Iterator<Item = Self> {
        ClientId::iter()
            .map(Self::for_client)
            .chain(std::iter::once(Self::synthetic()))
    }
}

fn parser_version(client: ClientId) -> u32 {
    match client {
        // These clients accumulated parser-only invalidations under the old
        // global schema. Their independent counters start from those histories
        // so future changes have an obvious local version to increment.
        // 5: Codex's `reasoning_output_tokens` is a subset of `output_tokens`
        // and is now split out of the `output` bucket instead of being carried
        // in both (see `CodexTotals::into_tokens`). Without this bump, an
        // unchanged transcript that already has a format-4 shard would keep
        // returning the pre-split `UnifiedMessage`, so historical rows would
        // stay double-priced. Codex has no retention path, so the bump only
        // makes the cache cold — everything is recoverable by re-parsing.
        // 6: Codex turn detection now also recognizes the `item_completed`
        // `UserMessage` payload that Codex 0.145+ emits in place of a
        // `user_message` event, so `is_turn_start` is set on transcripts that
        // previously produced none. Skipping this bump would confine the
        // correction to transcripts nobody has parsed yet: `get` treats a
        // matching version as a hit, and a rollout whose fingerprint has not
        // changed is never re-parsed, so every existing user would keep seeing
        // zero turns. Cold-rebuild only, for the same reason as 5.
        ClientId::Codex => 6,
        ClientId::Jcode => 4,
        // 5: Copilot Desktop's `events.jsonl` metadata reader now reads past a
        // non-UTF-8 line (`lossy_lines`) instead of stopping, and the desktop
        // DB entry can be served for an unchanged fingerprint, so only the
        // bump lets truncated metadata re-read.
        ClientId::Copilot => 5,
        // 4: session files are now read past a non-UTF-8 line instead of
        // stopping at the first one (upstream `cfe1304a`, #1031). A Grok
        // session file that is never appended to again keeps its fingerprint,
        // so only this bump discards the truncated parse.
        ClientId::Grok => 4,
        // 3: `.jsonl.zst` archives (and the `.jsonl.deleted.<ts>.zst` /
        // `.jsonl.reset.<ts>.zst` forms the scanner already matched) were read
        // as plain text and cached as empty; they now decode (upstream #1285).
        // Those archives never change on disk, so without the bump the empty
        // entries would be served forever. The same bump covers dropping
        // compaction checkpoint snapshots from discovery (upstream #1293).
        ClientId::OpenClaw => 3,
        // 2: the shared Roo/Kilo/Cline task-log parser now takes each entry's
        // `modelInfo` model (and provider when `apiProtocol` is silent), so
        // Cline 4.x tasks stop resolving to `unknown/unknown` (upstream
        // #1340). All three clients share `parse_roo_kilo_file`, so all three
        // re-parse.
        ClientId::RooCode | ClientId::KiloCode | ClientId::Cline => 2,
        // 3: `Input (w/ Cache Write)` is read as the cache-write bucket instead
        // of being reduced by `Input (w/o Cache Write)`, and any numeric
        // exported Cost (including an explicit `$0.00`) is provider-reported
        // (upstream #1154). Cached rows hold the old tokens and cost source.
        // No bump for usage-events JSON (#1247): the CSV parse is unchanged, and
        // the source cache is keyed per path, so a new `usage[.account].json`
        // has no entry to be stale.
        ClientId::Cursor => 3,
        // 2: Pi messages now carry a cross-session dedup key (upstream #1323),
        // and a cached v1 entry has none, so fork copies would keep counting
        // once per session file until re-parsed.
        // 3: the `pi` lane no longer scans `~/.omp/agent/sessions` (that root
        // is the `omp` client's now). Pi's parse is unchanged; the bump makes
        // existing Pi shards stale so entries cached for OMP files stop being
        // loaded on every scan and are dropped when the shards are rewritten.
        // Costs one cold re-parse of `~/.pi`.
        // Pi shares `parse_pi_format_file` with Kimchi, Senpi and Omp; a parse
        // change bumps all of them together.
        ClientId::Pi => 3,
        // 2: These parser output shapes now preserve source cost provenance;
        // Mux additionally splits mixed known/unknown token buckets.
        ClientId::Amp | ClientId::MiMoCode | ClientId::Mux => 2,
        // 2: Claude gained retention of history-only turns dropped by a
        // Claude Code transcript rewrite (see RET-CLAUDE-001 in
        // UPSTREAM.md). This records the parse-semantics change, not a
        // storage-layout change — the new `CachedSourceEntry::retained_keys`
        // field that retention needed is a shared bincode layout change
        // covered by `CACHE_FORMAT_VERSION` instead, since every namespace's
        // serialized entries gained the field, not just Claude's.
        //
        // 3: tool_result input tokens are no longer char-estimated. A bump
        // here used to discard retained-only turns; an entry behind the
        // current version is now kept and read through `retainable_history`.
        // Frozen payload migrations also preserve it across format bumps.
        //
        // 4: the cache write is split into its 1h and 5m portions, read from
        // `usage.cache_creation.ephemeral_1h_input_tokens`. Without this bump
        // the fix reaches nobody who already has a cache: `get` returns an
        // entry whose own `parser_version` matches as a hit, so a transcript
        // whose fingerprint has not changed is never re-parsed, and its
        // `cache_write_1h` stays zero and its cost stays at the old value.
        // Cold-cache verification passes either way, which is exactly why
        // the acceptance for this change is a warm-cache measurement.
        //
        // 5: bare prompt input no longer max-merges with cache-split input;
        // raw 1h writes are clamped only after duplicate assembly.
        ClientId::Claude => 5,
        // 2: Droid's cumulative session tokenUsage is now anchored on the
        // settings file's mtime (floored at providerLockTimestamp, via
        // parse_lock_timestamp/resolve_usage_timestamp) instead of the lock
        // timestamp alone, and — when the sibling transcript has assistant
        // replies — apportioned across them by context/output weight
        // (apportion) instead of emitted as one record per session. A
        // finished session's settings.json is never rewritten again, so its
        // fingerprint stays valid forever and only this bump discards a v1
        // entry's single lock-anchored record. Vendor-local numbering: never
        // copy upstream's parser_version 7 (see UPSTREAM.md).
        ClientId::Droid => 2,
        // 2: OpenCode 2.x renamed the session metadata table `session` ->
        // `session_v2`, so the v2 `session_message` parse now falls back to
        // the renamed join table. An unchanged database fingerprint whose
        // version-1 entry cached the dropped-empty result would otherwise
        // keep reporting zero OpenCode usage; the bump makes the namespace
        // cold so every source re-parses with the corrected query.
        ClientId::OpenCode => 2,
        // 1: ZCode's v2 SQLite parser (upstream `parse_zcode_sqlite`). An
        // explicit arm so the next ZCode parser change has a local counter
        // to increment instead of sharing the default.
        ClientId::Zcode => 1,
        // 1: Augment's session-snapshot parser (upstream `parse_augment_file`).
        ClientId::Augment => 1,
        // 1: Muse Code's session parser (upstream `parse_muse_file` at
        // `fe72e1f9`, which already carries #1355's provider metadata;
        // upstream's own counter is 3 — vendor-local numbering).
        ClientId::Muse => 1,
        // 1: Reasonix's stats parser (upstream `parse_reasonix_file`, which
        // upstream versions 4 after its #1095 reader change; vendor-local
        // numbering starts here because no older Reasonix entry exists).
        ClientId::Reasonix => 1,
        // 2: CLI sessions carrying Kiro credits now hold a provider-reported
        // cost, and IDE turns take their request count from `requestIds`
        // (upstream #1342, credits and message_count only; token counts
        // unchanged). A finished session's files never change again, so
        // without the bump its v1 entry would keep cost 0 and message_count 1
        // forever. The SQLite source gets the same changes but is parsed on
        // every scan, never cached. Vendor-local numbering.
        ClientId::Kiro => 2,
        // 1: Hindsight's usage-ledger parser (upstream `parse_hindsight_file`
        // plus the vendor-local `split_cached_input`).
        ClientId::Hindsight => 1,
        // 1: Kimchi's Pi-format parser (`parse_pi_format_file`, session-scoped
        // keys). Kimchi shares that parser with Pi: a parse change that bumps
        // `ClientId::Pi` must bump this arm too, or warm Kimchi caches keep
        // the old output.
        ClientId::Kimchi => 1,
        // 1: Senpi's Pi-format parser (`parse_pi_format_file`, cross-session
        // keys). Shared with Pi, Kimchi and Omp: a parse change bumps all four.
        ClientId::Senpi => 1,
        // 1: Oh My Pi's Pi-format parser (`parse_pi_format_file`,
        // cross-session keys). Shared with Pi, Kimchi and Senpi: bump all four
        // together.
        ClientId::Omp => 1,
        _ => 1,
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    namespace: String,
    path: CachedPath,
}

#[cfg(test)]
impl CacheKey {
    fn new(identity: CacheIdentity, path: &Path) -> Self {
        Self {
            namespace: identity.namespace.to_string(),
            path: CachedPath::from_path(path),
        }
    }

    fn shard(&self) -> CacheShardKey {
        CacheShardKey {
            namespace: self.namespace.clone(),
            index: legacy_shard_index(&self.namespace, &self.path),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheShardKey {
    namespace: String,
    index: usize,
}

/// The payload shape of `CACHE_FORMAT_VERSION` 3, kept so its shards can be
/// read after a bump rather than discarded.
///
/// Only `TokenBreakdown` differs from the current types -- format 4 appends
/// `cache_write_1h` to it -- but bincode is positional, so the whole chain
/// down to the field has to be spelled out to decode the old bytes.
///
/// A namespace with no retention loses nothing by going cold, so this exists
/// for the one that does: for Claude the cache is the only copy of turns a
/// compacting rewrite removed from the file, and a format bump would delete
/// them exactly as a parser_version bump does.
mod format3 {
    #[cfg(test)]
    use super::{CachedPath, CodexIncrementalCache, SourceFingerprint};
    use crate::sessions::CostSource;
    use serde::Deserialize;
    #[cfg(test)]
    use std::collections::HashSet;

    #[derive(Deserialize)]
    pub(super) struct TokenBreakdown {
        pub input: i64,
        pub output: i64,
        pub cache_read: i64,
        pub cache_write: i64,
        pub reasoning: i64,
    }

    #[derive(Deserialize)]
    pub(super) struct UnifiedMessage {
        pub client: String,
        pub model_id: String,
        pub provider_id: String,
        pub session_id: String,
        pub workspace_key: Option<String>,
        pub workspace_label: Option<String>,
        pub timestamp: i64,
        pub date: String,
        pub tokens: TokenBreakdown,
        pub cost: f64,
        pub cost_source: CostSource,
        pub duration_ms: Option<i64>,
        pub message_count: i32,
        pub agent: Option<String>,
        pub dedup_key: Option<String>,
        pub dedup_aliases: Vec<String>,
        pub is_turn_start: bool,
    }

    #[cfg(test)]
    #[derive(Deserialize)]
    pub(super) struct CachedSourceEntry {
        pub parser_namespace: String,
        pub parser_version: u32,
        pub path: CachedPath,
        pub fingerprint: SourceFingerprint,
        pub messages: Vec<UnifiedMessage>,
        pub fallback_timestamp_indices: Vec<usize>,
        pub codex_incremental: Option<CodexIncrementalCache>,
        pub retained_keys: HashSet<String>,
    }
}

/// Frozen format-4 payload: the six-lane token split plus the pre-provenance
/// `UnifiedMessage` tail. Decoded by recorded version, never by guessing; the
/// new `input_tokens_are_cache_split` reads `false` so retained-only history
/// cannot invent a split it never recorded.
pub(crate) mod format4 {
    #[cfg(test)]
    use super::{CachedPath, CodexIncrementalCache, SourceFingerprint};
    use crate::sessions::CostSource;
    use serde::{Deserialize, Serialize};
    #[cfg(test)]
    use std::collections::HashSet;

    #[derive(Deserialize, Serialize)]
    pub(super) struct TokenBreakdown {
        pub input: i64,
        pub output: i64,
        pub cache_read: i64,
        pub cache_write: i64,
        pub reasoning: i64,
        pub cache_write_1h: i64,
    }
    #[derive(Deserialize, Serialize)]
    pub(super) struct UnifiedMessage {
        pub client: String,
        pub model_id: String,
        pub provider_id: String,
        pub session_id: String,
        pub workspace_key: Option<String>,
        pub workspace_label: Option<String>,
        pub timestamp: i64,
        pub date: String,
        pub tokens: TokenBreakdown,
        pub cost: f64,
        pub cost_source: CostSource,
        pub duration_ms: Option<i64>,
        pub message_count: i32,
        pub agent: Option<String>,
        pub dedup_key: Option<String>,
        pub dedup_aliases: Vec<String>,
        pub is_turn_start: bool,
    }

    #[cfg(test)]
    #[derive(Deserialize, Serialize)]
    pub(super) struct CachedSourceEntry {
        pub parser_namespace: String,
        pub parser_version: u32,
        pub path: CachedPath,
        pub fingerprint: SourceFingerprint,
        pub messages: Vec<UnifiedMessage>,
        pub fallback_timestamp_indices: Vec<usize>,
        pub codex_incremental: Option<CodexIncrementalCache>,
        pub retained_keys: HashSet<String>,
    }
}
/// Frozen upstream tokscale format-10 Claude wire. Its token lane order and
/// entry tail differ from Syrtis format 5; never decode it as a local layout.
pub(crate) mod upstream10 {
    use crate::sessions::CostSource;
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize, Serialize)]
    pub(super) struct TokenBreakdown {
        pub input: i64,
        pub output: i64,
        pub cache_read: i64,
        pub cache_write: i64,
        pub cache_write_1h: i64,
        pub reasoning: i64,
    }

    #[derive(Deserialize, Serialize)]
    pub(super) struct UnifiedMessage {
        pub client: String,
        pub model_id: String,
        pub provider_id: String,
        pub session_id: String,
        pub workspace_key: Option<String>,
        pub workspace_label: Option<String>,
        pub timestamp: i64,
        pub date: String,
        pub tokens: TokenBreakdown,
        pub cost: f64,
        pub cost_source: CostSource,
        pub service_tier: Option<String>,
        pub duration_ms: Option<i64>,
        pub message_count: i32,
        pub agent: Option<String>,
        pub dedup_key: Option<String>,
        pub session_title: Option<String>,
        pub parent_session_id: Option<String>,
        pub is_turn_start: bool,
        pub model_attribution_conflicted: bool,
    }

    impl UnifiedMessage {
        pub(super) fn into_local(self) -> Result<crate::UnifiedMessage, String> {
            if self.client != "claude"
                || self.service_tier.is_some()
                || self.model_attribution_conflicted
            {
                return Err("unsupported upstream Claude message semantics".into());
            }
            Ok(crate::UnifiedMessage {
                client: self.client,
                model_id: self.model_id,
                provider_id: self.provider_id,
                session_id: self.session_id,
                workspace_key: self.workspace_key,
                workspace_label: self.workspace_label,
                timestamp: self.timestamp,
                date: self.date,
                tokens: crate::TokenBreakdown {
                    input: self.tokens.input,
                    output: self.tokens.output,
                    cache_read: self.tokens.cache_read,
                    cache_write: self.tokens.cache_write,
                    cache_write_1h: self.tokens.cache_write_1h,
                    reasoning: self.tokens.reasoning,
                },
                cost: self.cost,
                cost_source: self.cost_source,
                duration_ms: self.duration_ms,
                message_count: self.message_count,
                agent: self.agent,
                dedup_key: self.dedup_key,
                dedup_aliases: Vec::new(),
                is_turn_start: self.is_turn_start,
                // Upstream's wire carries no cache-split provenance.
                input_tokens_are_cache_split: false,
            })
        }
    }
}

impl From<format3::TokenBreakdown> for crate::TokenBreakdown {
    fn from(old: format3::TokenBreakdown) -> Self {
        Self {
            input: old.input,
            output: old.output,
            cache_read: old.cache_read,
            cache_write: old.cache_write,
            reasoning: old.reasoning,
            // Format 3 predates the split; the whole write is 5-minute as far
            // as anything that read those bytes was concerned.
            cache_write_1h: 0,
        }
    }
}

impl From<format4::TokenBreakdown> for crate::TokenBreakdown {
    fn from(old: format4::TokenBreakdown) -> Self {
        Self {
            input: old.input,
            output: old.output,
            cache_read: old.cache_read,
            cache_write: old.cache_write,
            reasoning: old.reasoning,
            cache_write_1h: old.cache_write_1h,
        }
    }
}

impl From<format3::UnifiedMessage> for UnifiedMessage {
    fn from(old: format3::UnifiedMessage) -> Self {
        Self {
            client: old.client,
            model_id: old.model_id,
            provider_id: old.provider_id,
            session_id: old.session_id,
            workspace_key: old.workspace_key,
            workspace_label: old.workspace_label,
            timestamp: old.timestamp,
            date: old.date,
            tokens: old.tokens.into(),
            cost: old.cost,
            cost_source: old.cost_source,
            duration_ms: old.duration_ms,
            message_count: old.message_count,
            agent: old.agent,
            dedup_key: old.dedup_key,
            dedup_aliases: old.dedup_aliases,
            is_turn_start: old.is_turn_start,
            input_tokens_are_cache_split: false,
        }
    }
}

impl From<format4::UnifiedMessage> for UnifiedMessage {
    fn from(old: format4::UnifiedMessage) -> Self {
        Self {
            client: old.client,
            model_id: old.model_id,
            provider_id: old.provider_id,
            session_id: old.session_id,
            workspace_key: old.workspace_key,
            workspace_label: old.workspace_label,
            timestamp: old.timestamp,
            date: old.date,
            tokens: old.tokens.into(),
            cost: old.cost,
            cost_source: old.cost_source,
            duration_ms: old.duration_ms,
            message_count: old.message_count,
            agent: old.agent,
            dedup_key: old.dedup_key,
            dedup_aliases: old.dedup_aliases,
            is_turn_start: old.is_turn_start,
            input_tokens_are_cache_split: false,
        }
    }
}

#[cfg(test)]
impl From<format3::CachedSourceEntry> for CachedSourceEntry {
    fn from(old: format3::CachedSourceEntry) -> Self {
        Self {
            // The entry keeps the version that wrote it. `get` filters on that
            // and so hides it from cache hits -- this build did not produce
            // these messages -- while `retainable_history` does not filter and
            // still reaches the turns only this copy holds.
            parser_namespace: old.parser_namespace,
            parser_version: old.parser_version,
            scope: [0; 32],
            path: old.path,
            fingerprint: old.fingerprint,
            messages: old.messages.into_iter().map(Into::into).collect(),
            fallback_timestamp_indices: old.fallback_timestamp_indices,
            codex_incremental: old.codex_incremental,
            retained_keys: old.retained_keys,
        }
    }
}

#[cfg(test)]
impl From<format4::CachedSourceEntry> for CachedSourceEntry {
    fn from(old: format4::CachedSourceEntry) -> Self {
        Self {
            // Same version-preserving contract as the format-3 migration:
            // hidden from cache hits, reachable as retained-only history.
            parser_namespace: old.parser_namespace,
            parser_version: old.parser_version,
            scope: [0; 32],
            path: old.path,
            fingerprint: old.fingerprint,
            messages: old.messages.into_iter().map(Into::into).collect(),
            fallback_timestamp_indices: old.fallback_timestamp_indices,
            codex_incremental: old.codex_incremental,
            retained_keys: old.retained_keys,
        }
    }
}

/// Format admission for the legacy in-memory test reader.
#[cfg(test)]
pub(crate) fn format_version_is_migratable(namespace: &str, format_version: u32) -> bool {
    matches!(format_version, 3 | 4) && retained_history_key_filter(namespace).is_some()
}

#[cfg(test)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedSourceEntry {
    parser_namespace: String,
    parser_version: u32,
    /// SQLite owns context identity; legacy positional shards are unscoped.
    #[serde(skip)]
    scope: [u8; 32],
    pub path: CachedPath,
    pub fingerprint: SourceFingerprint,
    /// Not always a pure function of the source file. For a namespace that
    /// `retained_history_key_filter` covers, this can hold messages the live
    /// file no longer contains, and re-parsing will not reproduce them — the
    /// cache is the only copy. That is what makes a parser_version bump for
    /// those namespaces lossy rather than merely cold.
    pub messages: Vec<UnifiedMessage>,
    pub fallback_timestamp_indices: Vec<usize>,
    pub codex_incremental: Option<CodexIncrementalCache>,
    /// Dedup keys within `messages` that were carried forward by
    /// `retain_observed_messages` rather than produced by a live parse of
    /// this entry's fingerprinted bytes. Always empty for a namespace
    /// `retained_history_key_filter` does not cover. Persisted so the
    /// live-outranks-retained guarantee in `lib.rs` holds on a cache hit, not
    /// only on the scan that first computed it — see RET-CLAUDE-001 in
    /// UPSTREAM.md. This is the field whose addition forced the
    /// `CACHE_FORMAT_VERSION` bump to 3, since it changes bincode's
    /// positional layout for every namespace's serialized entries, not just
    /// Claude's.
    pub retained_keys: HashSet<String>,
}

#[cfg(test)]
impl CachedSourceEntry {
    pub(crate) fn new(
        identity: CacheIdentity,
        path: &Path,
        fingerprint: SourceFingerprint,
        messages: Vec<UnifiedMessage>,
        fallback_timestamp_indices: Vec<usize>,
        codex_incremental: Option<CodexIncrementalCache>,
    ) -> Self {
        Self {
            parser_namespace: identity.namespace.to_string(),
            parser_version: identity.parser_version,
            scope: identity.scope,
            path: CachedPath::from_path(path),
            fingerprint,
            messages,
            fallback_timestamp_indices,
            codex_incremental,
            retained_keys: HashSet::new(),
        }
    }

    /// Tag which of `self.messages`' dedup keys are retained carry-overs
    /// rather than the live parse's own output. Only the Claude retention
    /// lanes in `lib.rs` call this.
    pub(crate) fn with_retained_keys(mut self, retained_keys: HashSet<String>) -> Self {
        self.retained_keys = retained_keys;
        self
    }
}

/// The dedup-key filter for namespaces whose entries carry history the live
/// file may no longer contain, or `None` for namespaces that do not retain
/// history.
///
/// Mirrors the `HistoryRetention` choice each lane makes in `lib.rs`. It has
/// to exist here as well because the save merge is the other place a retained
/// message can be dropped, and it must honor the same contract.
fn retained_history_key_filter(namespace: &str) -> Option<fn(&str) -> bool> {
    (namespace == ClientId::Claude.as_str())
        .then_some(crate::sessions::claudecode::dedup_key_is_globally_stable)
}

/// Old parser revisions whose supported shard layout can preserve facts.
///
/// Claude's version 1 predates `retained_keys` and so predates the format
/// 2 -> 3 layout change: its payload has one field fewer than the current
/// struct, whatever the format gate says.
fn oldest_migratable_parser_version(namespace: &str) -> Option<u32> {
    if namespace == ClientId::Claude.as_str() {
        // Claude version 1 predates the retained_keys field.
        Some(2)
    } else {
        CacheIdentity::current_for_namespace(namespace).map(|_| 1)
    }
}

/// Whether an entry on `parser_version` can be carried across to the current
/// one. A downgrade cannot: this build cannot know what a newer one wrote.
fn parser_version_is_migratable(namespace: &str, parser_version: u32) -> bool {
    CacheIdentity::current_for_namespace(namespace)
        .is_some_and(|current| parser_version < current.parser_version)
        && oldest_migratable_parser_version(namespace)
            .is_some_and(|oldest| parser_version >= oldest)
}

/// The envelope is deliberately independent from CachedSourceEntry's binary
/// layout. A parser version can therefore be checked before its payload is
/// deserialized, so (for example) a CodexParseState layout change cannot make
/// Claude's independently sharded cache unreadable.
///
/// For a retaining namespace the version here is the writer's, not a claim
/// about the contents: a rewrite carries migratable entries forward at their
/// own version. Readers judge `CachedSourceEntry::parser_version` instead.
#[cfg(test)]
#[derive(Debug, Serialize, Deserialize)]
struct CachedShardEnvelope {
    format_version: u32,
    parser_namespace: String,
    parser_version: u32,
    payload: Vec<u8>,
}

#[path = "message_cache_disk.rs"]
mod disk;

#[cfg(test)]
pub(crate) use disk::set_parser_version_for_test;
pub(crate) use disk::{
    collect_unreferenced_versions_on, decode_path_for_carry, decode_stored_message_for_query,
    encode_path_for_carry, import_legacy_shards_on, install_logical_views_on,
    reconcile_missing_sources_on, recover_source_message_cache, retained_state_change_token,
    validate_durable_store_on, validate_scope_metadata_on, CachedSourceMetadata,
    SourceMessageCache,
};

#[cfg(test)]
fn shard_filename(index: usize) -> String {
    format!("shard-{index:02x}.bin")
}

pub(crate) fn parse_shard_filename_for_import(filename: &std::ffi::OsStr) -> Option<usize> {
    parse_shard_filename(filename)
}

fn parse_shard_filename(filename: &std::ffi::OsStr) -> Option<usize> {
    let filename = filename.to_str()?;
    let encoded = filename.strip_prefix("shard-")?.strip_suffix(".bin")?;
    let index = usize::from_str_radix(encoded, 16).ok()?;
    (index < CACHE_SHARD_COUNT).then_some(index)
}
#[cfg(test)]
fn shard_path(root: &Path, key: &CacheShardKey) -> PathBuf {
    root.join(&key.namespace).join(shard_filename(key.index))
}

#[cfg(test)]
enum ShardReadStatus {
    Missing,
    Stale,
    Invalid(String),
    Loaded(Vec<CachedSourceEntry>),
}

#[cfg(test)]
fn read_shard(path: &Path, identity: CacheIdentity) -> ShardReadStatus {
    read_shard_with_limit(path, identity, MAX_CACHE_SHARD_BYTES)
}

#[cfg(test)]
fn read_shard_with_limit(
    path: &Path,
    identity: CacheIdentity,
    max_shard_bytes: u64,
) -> ShardReadStatus {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ShardReadStatus::Missing
        }
        Err(error) => return ShardReadStatus::Invalid(error.to_string()),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return ShardReadStatus::Invalid(error.to_string()),
    };
    if metadata.len() > max_shard_bytes {
        return ShardReadStatus::Invalid(format!(
            "{} bytes exceeds the {}-byte shard limit",
            metadata.len(),
            max_shard_bytes
        ));
    }

    let envelope: CachedShardEnvelope = match bincode::options()
        .with_limit(max_shard_bytes)
        .deserialize_from(BufReader::new(file))
    {
        Ok(envelope) => envelope,
        Err(error) => return ShardReadStatus::Invalid(error.to_string()),
    };
    // A retaining namespace's older shard is decoded through its own layout
    // rather than dropped. Rejecting it here is what makes a format bump lose
    // the turns a compacting rewrite already took out of the source file.
    let migrating_format = envelope.format_version != CACHE_FORMAT_VERSION;
    if migrating_format
        && !format_version_is_migratable(&envelope.parser_namespace, envelope.format_version)
    {
        return ShardReadStatus::Stale;
    }
    if envelope.parser_namespace != identity.namespace {
        return ShardReadStatus::Stale;
    }
    // An older version of a retaining namespace is decoded rather than
    // dropped; each entry carries its own version, so the caller judges them
    // one at a time.
    if envelope.parser_version != identity.parser_version
        && !parser_version_is_migratable(&envelope.parser_namespace, envelope.parser_version)
    {
        return ShardReadStatus::Stale;
    }

    if migrating_format && envelope.format_version == 3 {
        return match bincode::options()
            .with_limit(max_shard_bytes)
            .deserialize::<Vec<format3::CachedSourceEntry>>(&envelope.payload)
        {
            Ok(entries) => ShardReadStatus::Loaded(entries.into_iter().map(Into::into).collect()),
            Err(error) => ShardReadStatus::Invalid(error.to_string()),
        };
    }
    if migrating_format && envelope.format_version == 4 {
        return match bincode::options()
            .with_limit(max_shard_bytes)
            .deserialize::<Vec<format4::CachedSourceEntry>>(&envelope.payload)
        {
            Ok(entries) => ShardReadStatus::Loaded(entries.into_iter().map(Into::into).collect()),
            Err(error) => ShardReadStatus::Invalid(error.to_string()),
        };
    }

    match bincode::options()
        .with_limit(max_shard_bytes)
        .deserialize(&envelope.payload)
    {
        Ok(entries) => ShardReadStatus::Loaded(entries),
        Err(error) => ShardReadStatus::Invalid(error.to_string()),
    }
}

fn read_sample_hash(file: &mut File, offset: u64, len: usize) -> Option<FileSampleHash> {
    if len == 0 {
        return Some(FileSampleHash {
            offset,
            len: 0,
            hash: 0,
        });
    }

    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0_u8; len];
    file.read_exact(&mut buffer).ok()?;

    Some(FileSampleHash {
        offset,
        len: len as u64,
        hash: hash_bytes(&buffer),
    })
}

fn compute_sample_hashes(path: &Path, size: u64) -> Option<Vec<FileSampleHash>> {
    if size == 0 {
        return Some(Vec::new());
    }

    let mut file = File::open(path).ok()?;
    let offsets = sample_offsets(size);
    offsets
        .into_iter()
        .map(|(offset, len)| read_sample_hash(&mut file, offset, len))
        .collect()
}

pub(crate) fn sample_offsets(size: u64) -> Vec<(u64, usize)> {
    let sample_len = size.min(FINGERPRINT_SAMPLE_BYTES as u64) as usize;
    if sample_len == 0 {
        return Vec::new();
    }

    let max_offset = size.saturating_sub(sample_len as u64);
    let mut offsets = if max_offset == 0 {
        vec![0]
    } else {
        vec![
            0,
            max_offset / 4,
            max_offset / 2,
            max_offset.saturating_mul(3) / 4,
            max_offset,
        ]
    };
    offsets.sort_unstable();
    offsets.dedup();
    offsets.truncate(FINGERPRINT_SAMPLE_POINTS);
    offsets
        .into_iter()
        .map(|offset| (offset, sample_len))
        .collect()
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Whether a fingerprint carries a whole-file `content_hash`.
///
/// Validation uses size + mtime + samples ([`primary_fingerprint_matches`] and
/// [`related_fingerprint_metadata_matches`]) for every source. Only Codex reads
/// `content_hash` for incremental resume;
/// generic parsers and SQLite sources store a zero sentinel so changed or cold
/// files do not pay for a second whole-file hash that cannot affect parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentHashMode {
    Full,
    SamplesOnly,
}

fn file_fingerprint_parts(
    path: &Path,
    mode: ContentHashMode,
) -> Option<(u64, u64, Vec<FileSampleHash>, [u8; 32])> {
    let metadata = path.metadata().ok()?;
    let size = metadata.len();
    let modified_ns = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    let sample_hashes = compute_sample_hashes(path, size)?;
    let content_hash = match mode {
        ContentHashMode::Full => hash_prefix(path, size)?,
        ContentHashMode::SamplesOnly => [0_u8; 32],
    };
    Some((size, modified_ns, sample_hashes, content_hash))
}

pub(crate) fn append_path_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Compatibility wrapper for the local live-tail mtime probe.
pub(crate) fn jcode_journal_path(path: &Path) -> PathBuf {
    crate::sessions::jcode::jcode_journal_path(path)
}

pub(crate) fn hash_prefix(path: &Path, len: u64) -> Option<[u8; 32]> {
    #[cfg(test)]
    FULL_HASH_CALLS.with(|calls| calls.set(calls.get() + 1));

    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];

    while remaining > 0 {
        let bytes_to_read = remaining.min(HASH_BUFFER_BYTES as u64) as usize;
        let read = file.read(&mut buffer[..bytes_to_read]).ok()?;
        if read == 0 {
            return None;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }

    Some(hasher.finalize().into())
}

#[cfg(test)]
fn full_hash_call_count() -> usize {
    FULL_HASH_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn build_codex_incremental_cache(
    path: &Path,
    consumed_offset: u64,
    state: CodexParseState,
) -> Option<CodexIncrementalCache> {
    let ends_with_newline = consumed_offset == 0 || file_ends_with_newline(path, consumed_offset);
    if !ends_with_newline {
        return None;
    }

    Some(CodexIncrementalCache {
        state,
        consumed_offset,
        ends_with_newline,
        prefix_hash: hash_prefix(path, consumed_offset)?,
    })
}

/// Build Codex incremental state when the caller already hashed the complete
/// consumed prefix. Full-file Codex fingerprints are also the prefix hash when
/// `consumed_offset` equals the current file size, so accepting that digest
/// avoids a second read of the transcript.
pub(crate) fn build_codex_incremental_cache_with_prefix_hash(
    path: &Path,
    consumed_offset: u64,
    state: CodexParseState,
    prefix_hash: [u8; 32],
) -> Option<CodexIncrementalCache> {
    let ends_with_newline = consumed_offset == 0 || file_ends_with_newline(path, consumed_offset);
    if !ends_with_newline {
        return None;
    }

    Some(CodexIncrementalCache {
        state,
        consumed_offset,
        ends_with_newline,
        prefix_hash,
    })
}

fn file_ends_with_newline(path: &Path, size: u64) -> bool {
    if size == 0 {
        return true;
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.seek(SeekFrom::Start(size.saturating_sub(1))).is_err() {
        return false;
    }

    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).is_ok() && byte[0] == b'\n'
}

pub(crate) fn codex_prefix_matches(path: &Path, cached: &CodexIncrementalCache) -> bool {
    if cached.consumed_offset > 0 && !cached.ends_with_newline {
        return false;
    }

    match hash_prefix(path, cached.consumed_offset) {
        Some(prefix_hash) => prefix_hash == cached.prefix_hash,
        None => false,
    }
}

#[cfg(test)]
pub(crate) fn codex_cache_entry_matches_fingerprint(
    cached: &CachedSourceEntry,
    fingerprint: &SourceFingerprint,
) -> bool {
    let Some(codex_incremental) = cached.codex_incremental.as_ref() else {
        return false;
    };

    codex_incremental.consumed_offset == fingerprint.size
        && codex_incremental.ends_with_newline
        && codex_incremental.prefix_hash == fingerprint.content_hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TokenBreakdown;
    use std::io::Write;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::{NamedTempFile, TempDir};

    #[test]
    fn from_roo_path_invalidates_on_history_only_change() {
        // parse_roo_kilo_file reads model/agent from the sibling
        // api_conversation_history.json, so a history-only rewrite (ui_messages
        // byte-identical) must change the fingerprint or the cache serves stale
        // model/agent/pricing.
        let dir = TempDir::new().unwrap();
        let ui = dir.path().join("ui_messages.json");
        std::fs::write(&ui, b"[]").unwrap();
        let history = dir.path().join("api_conversation_history.json");
        std::fs::write(&history, b"<model>claude-sonnet-4</model>").unwrap();

        let roo_before = SourceFingerprint::from_roo_path(&ui).unwrap();
        let plain_before = SourceFingerprint::from_path(&ui).unwrap();

        // Rewrite the history only; leave ui_messages.json byte-identical.
        std::fs::write(&history, b"<model>claude-opus-4</model>").unwrap();

        let roo_after = SourceFingerprint::from_roo_path(&ui).unwrap();
        let plain_after = SourceFingerprint::from_path(&ui).unwrap();

        assert_ne!(
            roo_before, roo_after,
            "a history-only change must alter the roo fingerprint"
        );
        assert_eq!(
            plain_before, plain_after,
            "from_path ignores the history sibling (control)"
        );
    }

    fn restore_env_var(key: &str, value: Option<impl AsRef<std::ffi::OsStr>>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    const CACHE_ENV_KEYS: [&str; 7] = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "TOKSCALE_CONFIG_DIR",
        "SYRTIS_USAGE_DIR",
        "SYRTIS_CONFIG_DIR",
        "XDG_DATA_HOME",
    ];

    /// Pin every env var the cache resolvers consult so the test stays
    /// inside `temp_home`. CI runners can leak `XDG_CONFIG_HOME` /
    /// `XDG_CACHE_HOME` from the host, which would resolve cache shards outside
    /// the sandbox. `dirs::config_dir()` uses Windows Known Folders instead of
    /// HOME/XDG, so `TOKSCALE_CONFIG_DIR` must also be pinned explicitly there.
    /// Returns the previous values so the caller can restore.
    fn sandbox_cache_env(temp_home: &std::path::Path) -> [Option<std::ffi::OsString>; 7] {
        let previous = CACHE_ENV_KEYS.map(std::env::var_os);
        unsafe {
            std::env::set_var("HOME", temp_home);
            std::env::set_var("XDG_CONFIG_HOME", temp_home.join(".config"));
            std::env::set_var("XDG_CACHE_HOME", temp_home.join(".cache"));
            std::env::set_var("TOKSCALE_CONFIG_DIR", temp_home);
            std::env::set_var("SYRTIS_USAGE_DIR", temp_home.join("usage"));
            std::env::set_var("SYRTIS_CONFIG_DIR", temp_home.join("syrtis-config"));
            std::env::set_var("XDG_DATA_HOME", temp_home.join(".local/share"));
        }
        previous
    }

    fn restore_cache_env(previous: [Option<std::ffi::OsString>; 7]) {
        for (key, value) in CACHE_ENV_KEYS.into_iter().zip(previous) {
            restore_env_var(key, value);
        }
    }

    fn write_temp_file(content: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content).unwrap();
        file.flush().unwrap();
        file
    }

    fn test_entry(identity: CacheIdentity, path: &Path, session_id: &str) -> CachedSourceEntry {
        CachedSourceEntry::new(
            identity,
            path,
            SourceFingerprint::from_path(path).unwrap(),
            vec![UnifiedMessage::new(
                identity.namespace,
                "gpt-5",
                "provider",
                session_id,
                1,
                TokenBreakdown {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
            )],
            Vec::new(),
            None,
        )
    }

    fn keyed_message(namespace: &str, session_id: &str, dedup_key: &str) -> UnifiedMessage {
        UnifiedMessage::new_with_dedup(
            namespace,
            "claude-3-5-sonnet",
            "anthropic",
            session_id,
            1,
            TokenBreakdown {
                input: 1,
                output: 2,
                ..Default::default()
            },
            0.0,
            Some(dedup_key.to_string()),
        )
    }

    fn entry_with_messages(
        identity: CacheIdentity,
        path: &Path,
        messages: Vec<UnifiedMessage>,
    ) -> CachedSourceEntry {
        let fingerprint = SourceFingerprint::from_path(path).unwrap();
        let resume = (identity.namespace == ClientId::Codex.as_str())
            .then(|| {
                build_codex_incremental_cache(path, fingerprint.size, CodexParseState::default())
            })
            .flatten();
        CachedSourceEntry::new(identity, path, fingerprint, messages, Vec::new(), resume)
    }

    fn write_sources(dir: &TempDir) -> (PathBuf, PathBuf) {
        let first = dir.path().join("source-0.jsonl");
        let second = dir.path().join("source-1.jsonl");
        std::fs::write(&first, b"source-0\n").unwrap();
        std::fs::write(&second, b"source-1\n").unwrap();
        (first, second)
    }

    #[test]
    fn test_retained_state_change_token_tracks_only_report_visible_claude_state() {
        let source_dir = TempDir::new().unwrap();
        let cache_root = TempDir::new().unwrap();
        let source = source_dir.path().join("session.jsonl");
        std::fs::write(&source, b"source\n").unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude).with_scope(source_dir.path());
        let paths = std::slice::from_ref(&source);
        let token = || {
            retained_state_change_token(Some(cache_root.path()), paths, source_dir.path()).unwrap()
        };
        let empty = token();
        let mut cache = SourceMessageCache::load_from_root(Some(cache_root.path()));
        let mut entry = entry_with_messages(
            identity,
            &source,
            vec![keyed_message("claude", "session", "message:req")],
        );
        cache.insert(entry.clone());
        cache.check().unwrap();
        assert_eq!(
            token(),
            empty,
            "live-only state does not invalidate retained queries"
        );
        entry.retained_keys.insert("message:req".to_string());
        std::fs::write(&source, b"rewritten\n").unwrap();
        entry.fingerprint = SourceFingerprint::from_path(&source).unwrap();
        cache.insert(entry.clone());
        cache.check().unwrap();
        let retained = token();
        assert_ne!(retained, empty);
        assert_eq!(token(), retained);
        entry.messages[0].tokens.output = 99;
        cache.insert(entry);
        cache.check().unwrap();
        assert_ne!(token(), retained, "changed retained facts change the token");
        assert_eq!(
            retained_state_change_token(Some(cache_root.path()), &[], source_dir.path()).unwrap(),
            empty
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_retained_state_change_token_is_consistent_for_overlapping_readers() {
        let source_dir = TempDir::new().unwrap();
        let cache_root = TempDir::new().unwrap();
        let source = source_dir.path().join("session.jsonl");
        std::fs::write(&source, b"source\n").unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let dedup_key = "message:req";
        let mut retained_entry = entry_with_messages(
            identity,
            &source,
            vec![keyed_message("claude", "session", dedup_key)],
        );
        retained_entry.retained_keys.insert(dedup_key.to_string());

        let mut cache = SourceMessageCache::load_from_root(Some(cache_root.path()));
        cache.insert(retained_entry.clone());

        let run_overlapping = |root: &Path, source: &Path| {
            let barrier = Arc::new(Barrier::new(3));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let root = root.to_path_buf();
                    let source = source.to_path_buf();
                    thread::spawn(move || {
                        barrier.wait();
                        retained_state_change_token(
                            Some(root.as_path()),
                            &[source],
                            Path::new("test-home"),
                        )
                        .unwrap()
                    })
                })
                .collect();
            barrier.wait();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        };

        let first_tokens = run_overlapping(cache_root.path(), &source);
        assert_eq!(first_tokens[0], first_tokens[1]);

        let mut replacement = retained_entry;
        replacement.messages[0].tokens.output = 999_999;
        cache.insert(replacement);

        let changed_tokens = run_overlapping(cache_root.path(), &source);
        assert_ne!(first_tokens[0], changed_tokens[0]);
        assert_eq!(changed_tokens[0], changed_tokens[1]);

        assert_eq!(
            changed_tokens[0],
            retained_state_change_token(
                Some(cache_root.path()),
                std::slice::from_ref(&source),
                Path::new("test-home")
            )
            .unwrap()
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_resolved_root_survives_fail_soft_load_without_live_fallback() {
        let profile_a = TempDir::new().unwrap();
        let profile_b = TempDir::new().unwrap();
        let captured_root = profile_a.path().join("cache");
        std::fs::create_dir_all(&captured_root).unwrap();
        std::fs::write(
            captured_root.join(CACHE_SHARD_DIRNAME),
            b"not a directory\n",
        )
        .unwrap();

        let prev_env = sandbox_cache_env(profile_a.path());
        let mut cache = SourceMessageCache::load_from_root(Some(&captured_root));
        let retained_root = cache.root().unwrap().to_path_buf();

        unsafe {
            std::env::set_var("TOKSCALE_CONFIG_DIR", profile_b.path());
        }
        let source = write_temp_file(b"source\n");
        cache.insert(test_entry(
            CacheIdentity::for_client(ClientId::Claude),
            source.path(),
            "session",
        ));
        assert!(cache.check().is_err());
        let live_cache_created = profile_b.path().join("cache").exists();
        restore_cache_env(prev_env);

        assert_eq!(retained_root, captured_root);
        assert!(
            !live_cache_created,
            "a cache write must not fall back to the live profile after a resolved-root load fails"
        );
    }

    #[test]
    fn test_codex_prefix_matches_appended_file() {
        let file = write_temp_file(b"line-1\nline-2\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        let mut reopened = file.reopen().unwrap();
        reopened.seek(SeekFrom::End(0)).unwrap();
        reopened.write_all(b"line-3\n").unwrap();
        reopened.flush().unwrap();

        assert!(codex_prefix_matches(file.path(), &incremental_cache,));
    }

    #[test]
    fn test_codex_incremental_cache_reuses_full_hash() {
        let file = write_temp_file(b"line-1\nline-2\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let full_hashes_before = full_hash_call_count();

        let incremental_cache = build_codex_incremental_cache_with_prefix_hash(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
            fingerprint.content_hash,
        )
        .unwrap();

        assert_eq!(
            full_hash_call_count(),
            full_hashes_before,
            "a supplied Codex fingerprint must avoid a second whole-file SHA-256"
        );
        assert_eq!(incremental_cache.prefix_hash, fingerprint.content_hash);
        assert!(incremental_cache.ends_with_newline);
    }

    #[test]
    fn test_check_path_returns_unchanged_for_matching_metadata_and_samples() {
        let file = write_temp_file(&vec![b'a'; 32 * 1024]);
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let full_hashes_before = full_hash_call_count();

        let status = SourceFingerprint::check_path(file.path(), Some(&fingerprint)).unwrap();

        assert!(matches!(status, FingerprintStatus::Unchanged));
        assert_eq!(
            full_hash_call_count(),
            full_hashes_before,
            "an unchanged fingerprint must not compute a full SHA-256"
        );
    }

    #[test]
    fn test_check_path_returns_changed_when_sample_changes_with_same_metadata() {
        let original = vec![b'a'; 32 * 1024];
        let file = write_temp_file(&original);
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let original_signature = metadata_signature(file.path()).unwrap();
        let original_modified = std::fs::metadata(file.path()).unwrap().modified().unwrap();

        let mut rewritten = original;
        rewritten[0] = b'z';
        std::fs::write(file.path(), rewritten).unwrap();
        File::options()
            .write(true)
            .open(file.path())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        assert_eq!(metadata_signature(file.path()).unwrap(), original_signature);
        let full_hashes_before = full_hash_call_count();

        let status = SourceFingerprint::check_path(file.path(), Some(&fingerprint)).unwrap();

        let FingerprintStatus::Changed(changed) = status else {
            panic!("changed sample must rebuild the full fingerprint");
        };
        assert_ne!(changed, fingerprint);
        assert_eq!(
            full_hash_call_count(),
            full_hashes_before + 1,
            "a changed sample must rebuild the full fingerprint"
        );
    }

    #[test]
    fn test_generic_sources_skip_full_hash() {
        let original = vec![b'a'; 64 * 1024];
        let file = write_temp_file(&original);
        let fingerprint = SourceFingerprint::from_path_samples_only(file.path()).unwrap();
        let original_signature = metadata_signature(file.path()).unwrap();
        let original_modified = std::fs::metadata(file.path()).unwrap().modified().unwrap();

        let mut rewritten = original;
        rewritten[0] = b'z';
        std::fs::write(file.path(), rewritten).unwrap();
        File::options()
            .write(true)
            .open(file.path())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();
        assert_eq!(metadata_signature(file.path()).unwrap(), original_signature);

        let full_hashes_before = full_hash_call_count();
        let changed = SourceFingerprint::from_path_samples_only(file.path()).unwrap();
        assert_ne!(changed, fingerprint);
        assert_eq!(
            full_hash_call_count(),
            full_hashes_before,
            "generic source fingerprints must not compute a whole-file SHA-256"
        );
        assert_eq!(changed.content_hash, [0_u8; 32]);
    }

    #[test]
    fn test_sqlite_fingerprint_skips_full_hash() {
        let file = write_temp_file(&vec![b'a'; 64 * 1024]);
        let full_hashes_before = full_hash_call_count();

        let fingerprint = SourceFingerprint::from_sqlite_path(file.path()).unwrap();

        assert_eq!(
            full_hash_call_count(),
            full_hashes_before,
            "a SQLite fingerprint must not compute a whole-file SHA-256"
        );
        assert_eq!(
            fingerprint.content_hash, [0_u8; 32],
            "a SQLite fingerprint stores a zero content_hash sentinel"
        );
        assert!(
            !fingerprint.sample_hashes.is_empty(),
            "samples still guard SQLite change detection"
        );
    }

    #[test]
    fn test_sqlite_check_detects_change_without_full_hash() {
        let original = vec![b'a'; 64 * 1024];
        let file = write_temp_file(&original);
        let fingerprint = SourceFingerprint::from_sqlite_path(file.path()).unwrap();

        // Unchanged: metadata + samples match, no full hash.
        let full_hashes_before = full_hash_call_count();
        let status = SourceFingerprint::check_sqlite_path(file.path(), Some(&fingerprint)).unwrap();
        assert!(matches!(status, FingerprintStatus::Unchanged));

        // Changed: a same-size rewrite with a rolled-back mtime is still caught
        // by the samples, and still without a whole-file hash.
        let original_modified = std::fs::metadata(file.path()).unwrap().modified().unwrap();
        let mut rewritten = original;
        rewritten[0] = b'z';
        std::fs::write(file.path(), rewritten).unwrap();
        File::options()
            .write(true)
            .open(file.path())
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_modified))
            .unwrap();

        let status = SourceFingerprint::check_sqlite_path(file.path(), Some(&fingerprint)).unwrap();
        assert!(matches!(status, FingerprintStatus::Changed(_)));
        assert_eq!(
            full_hash_call_count(),
            full_hashes_before,
            "SQLite change detection must never compute a whole-file SHA-256"
        );
    }

    #[test]
    fn test_source_fingerprint_changes_for_same_size_rewrite() {
        let file = write_temp_file(b"aaaa\nbbbb\ncccc\n");
        let before = SourceFingerprint::from_path(file.path()).unwrap();

        std::fs::write(file.path(), b"aaaa\nzzzz\ncccc\n").unwrap();

        let after = SourceFingerprint::from_path(file.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn test_source_fingerprint_changes_for_large_same_size_unsampled_rewrite() {
        let mut original = vec![b'a'; 128 * 1024];
        original.extend_from_slice(b"\n");
        let file = write_temp_file(&original);
        let before = SourceFingerprint::from_path(file.path()).unwrap();

        let mut rewritten = original.clone();
        rewritten[73 * 1024] = b'z';
        std::fs::write(file.path(), &rewritten).unwrap();

        let after = SourceFingerprint::from_path(file.path()).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn test_sqlite_source_fingerprint_tracks_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("history.db");
        std::fs::write(&db_path, b"main-db").unwrap();

        let base = SourceFingerprint::from_sqlite_path(&db_path).unwrap();

        let wal_path = append_path_suffix(&db_path, "-wal");
        std::fs::write(&wal_path, b"wal-1").unwrap();
        let with_wal = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_ne!(base, with_wal);

        std::fs::write(&wal_path, b"wal-2").unwrap();
        let updated_wal = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_ne!(with_wal, updated_wal);

        let before_shm = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        let shm_path = append_path_suffix(&db_path, "-shm");
        std::fs::write(&shm_path, b"shm-1").unwrap();
        let with_shm = SourceFingerprint::from_sqlite_path(&db_path).unwrap();
        assert_eq!(before_shm, with_shm);
    }

    #[test]
    fn test_copilot_desktop_dynamic_event_set_adds_and_removes() {
        let dir = TempDir::new().unwrap();
        let copilot_root = dir.path().join(".copilot");
        let db_path = copilot_root.join("copilot.db");
        std::fs::create_dir_all(&copilot_root).unwrap();
        std::fs::write(&db_path, b"database\n").unwrap();

        let without_events = SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        let event_path = copilot_root
            .join("session-state/session-1")
            .join("events.jsonl");
        std::fs::create_dir_all(event_path.parent().unwrap()).unwrap();
        std::fs::write(&event_path, b"event\n").unwrap();
        assert!(matches!(
            SourceFingerprint::check_copilot_desktop_path_samples_only(
                &db_path,
                Some(&without_events),
            ),
            Some(FingerprintStatus::Changed(_))
        ));

        let with_events = SourceFingerprint::from_copilot_desktop_path(&db_path).unwrap();
        std::fs::remove_file(&event_path).unwrap();
        assert!(matches!(
            SourceFingerprint::check_copilot_desktop_path_samples_only(
                &db_path,
                Some(&with_events),
            ),
            Some(FingerprintStatus::Changed(_))
        ));
    }

    #[test]
    fn test_related_path_identity_and_set_changes_miss_warm_cache() {
        let dir = TempDir::new().unwrap();
        let primary_path = dir.path().join("primary.jsonl");
        let first_related = dir.path().join("first.json");
        let second_related = dir.path().join("second.json");
        std::fs::write(&primary_path, b"primary\n").unwrap();
        std::fs::write(&first_related, b"same\n").unwrap();
        std::fs::write(&second_related, b"same\n").unwrap();
        let first_modified = std::fs::metadata(&first_related)
            .unwrap()
            .modified()
            .unwrap();
        File::options()
            .write(true)
            .open(&second_related)
            .unwrap()
            .set_modified(first_modified)
            .unwrap();

        let cached = SourceFingerprint::from_path_with_related_mode(
            &primary_path,
            vec![("dependency".to_string(), first_related.clone())],
            ContentHashMode::SamplesOnly,
        )
        .unwrap();
        assert!(matches!(
            SourceFingerprint::check_path_with_related_mode(
                &primary_path,
                vec![("dependency".to_string(), first_related.clone())],
                Some(&cached),
                ContentHashMode::SamplesOnly,
            ),
            Some(FingerprintStatus::Unchanged)
        ));

        assert!(matches!(
            SourceFingerprint::check_path_with_related_mode(
                &primary_path,
                vec![("dependency".to_string(), second_related.clone())],
                Some(&cached),
                ContentHashMode::SamplesOnly,
            ),
            Some(FingerprintStatus::Changed(_))
        ));
        assert!(matches!(
            SourceFingerprint::check_path_with_related_mode(
                &primary_path,
                vec![
                    ("dependency".to_string(), first_related),
                    ("extra".to_string(), second_related),
                ],
                Some(&cached),
                ContentHashMode::SamplesOnly,
            ),
            Some(FingerprintStatus::Changed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_related_non_not_found_read_failure_returns_none() {
        let dir = TempDir::new().unwrap();
        let primary_path = dir.path().join("primary.jsonl");
        let related_path = dir.path().join("dependency.json");
        std::fs::write(&primary_path, b"primary\n").unwrap();
        std::fs::write(&related_path, b"dependency\n").unwrap();
        let mut cached = SourceFingerprint::from_path_with_related_mode(
            &primary_path,
            vec![("dependency".to_string(), related_path.clone())],
            ContentHashMode::SamplesOnly,
        )
        .unwrap();

        std::fs::remove_file(&related_path).unwrap();
        std::fs::create_dir(&related_path).unwrap();
        let (size, modified_ns) = metadata_signature(&related_path).unwrap();
        cached.related_files[0].size = size;
        cached.related_files[0].modified_ns = modified_ns;
        assert!(
            SourceFingerprint::check_path_with_related_mode(
                &primary_path,
                vec![("dependency".to_string(), related_path)],
                Some(&cached),
                ContentHashMode::SamplesOnly,
            )
            .is_none(),
            "a non-NotFound related read failure must fail open to a cold parse"
        );
    }

    #[test]
    fn test_jcode_fingerprint_tracks_journal_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let session_path = dir.path().join("session_fixture.json");
        std::fs::write(&session_path, br#"{"messages":[]}"#).unwrap();

        let base = SourceFingerprint::from_jcode_path(&session_path).unwrap();

        let journal_path = dir.path().join("session_fixture.journal.jsonl");
        std::fs::write(
            &journal_path,
            br#"{"append_messages":[]}
"#,
        )
        .unwrap();
        let with_journal = SourceFingerprint::from_jcode_path(&session_path).unwrap();
        assert_ne!(base, with_journal);

        std::fs::write(
            &journal_path,
            br#"{"append_messages":[{"id":"assistant_1"}]}
"#,
        )
        .unwrap();
        let updated_journal = SourceFingerprint::from_jcode_path(&session_path).unwrap();
        assert_ne!(with_journal, updated_journal);
    }

    #[test]
    fn test_grok_fingerprint_tracks_signals_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let updates_path = dir.path().join("updates.jsonl");
        std::fs::write(&updates_path, b"update\n").unwrap();

        let base = SourceFingerprint::from_grok_path(&updates_path).unwrap();

        let signals_path = dir.path().join("signals.json");
        std::fs::write(&signals_path, br#"{"input":1}"#).unwrap();
        let with_signals = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(base, with_signals);

        std::fs::write(&signals_path, br#"{"input":2}"#).unwrap();
        let updated_signals = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(with_signals, updated_signals);
    }

    #[test]
    fn test_grok_fingerprint_tracks_summary_and_events_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let updates_path = dir.path().join("updates.jsonl");
        std::fs::write(&updates_path, b"update\n").unwrap();

        let base = SourceFingerprint::from_grok_path(&updates_path).unwrap();

        let summary_path = dir.path().join("summary.json");
        std::fs::write(&summary_path, br#"{"model":"grok-3"}"#).unwrap();
        let with_summary = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(base, with_summary);

        std::fs::write(&summary_path, br#"{"model":"grok-4"}"#).unwrap();
        let updated_summary = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(with_summary, updated_summary);

        let events_path = dir.path().join("events.jsonl");
        std::fs::write(&events_path, b"event-1\n").unwrap();
        let with_events = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(updated_summary, with_events);

        std::fs::write(&events_path, b"event-2\n").unwrap();
        let updated_events = SourceFingerprint::from_grok_path(&updates_path).unwrap();
        assert_ne!(with_events, updated_events);
    }

    #[test]
    fn test_kiro_ide_fingerprint_tracks_messages_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let sess_dir = dir.path().join("workspace-a/sess_02f1c107");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let session_path = sess_dir.join("session.json");
        std::fs::write(&session_path, br#"{"schemaVersion":"1.0.0"}"#).unwrap();

        let base = SourceFingerprint::from_kiro_path(&session_path).unwrap();

        // messages.jsonl appearing (session.json untouched) must invalidate.
        let messages_path = sess_dir.join("messages.jsonl");
        std::fs::write(
            &messages_path,
            br#"{"role":"user","content":"hello"}
"#,
        )
        .unwrap();
        let with_messages = SourceFingerprint::from_kiro_path(&session_path).unwrap();
        assert_ne!(base, with_messages);

        // An append landing after the last session.json write must invalidate.
        std::fs::write(
            &messages_path,
            br#"{"role":"user","content":"hello"}
{"role":"assistant","content":"world"}
"#,
        )
        .unwrap();
        let updated_messages = SourceFingerprint::from_kiro_path(&session_path).unwrap();
        assert_ne!(with_messages, updated_messages);

        // A CLI source records its absent same-stem JSONL sidecar so a later
        // creation invalidates the cache without reparsing the primary file.
        let cli_path = dir.path().join("cli-session.json");
        std::fs::write(&cli_path, b"{}").unwrap();
        let cli_fingerprint = SourceFingerprint::from_kiro_path(&cli_path).unwrap();
        assert!(cli_fingerprint.related_files.iter().any(|related| {
            related.suffix == "messages.jsonl"
                && related.path.to_path_buf() == dir.path().join("cli-session.jsonl")
                && !related.exists
        }));
    }

    #[test]
    fn test_kiro_cli_fingerprint_tracks_same_stem_jsonl_changes() {
        let dir = TempDir::new().unwrap();
        let session_path = dir.path().join("cli-session.json");
        std::fs::write(&session_path, br#"{"sessionId":"session-1"}"#).unwrap();

        let base = SourceFingerprint::from_kiro_path(&session_path).unwrap();
        assert!(matches!(
            SourceFingerprint::check_kiro_path_samples_only(&session_path, Some(&base)),
            Some(FingerprintStatus::Unchanged)
        ));

        let messages_path = dir.path().join("cli-session.jsonl");
        std::fs::write(&messages_path, b"message-1\n").unwrap();
        assert!(matches!(
            SourceFingerprint::check_kiro_path_samples_only(&session_path, Some(&base)),
            Some(FingerprintStatus::Changed(_))
        ));
        let with_messages = SourceFingerprint::from_kiro_path(&session_path).unwrap();
        assert_ne!(base, with_messages);

        std::fs::write(&messages_path, b"message-2\n").unwrap();
        let updated_messages = SourceFingerprint::from_kiro_path(&session_path).unwrap();
        assert_ne!(with_messages, updated_messages);

        std::fs::remove_file(&messages_path).unwrap();
        assert!(matches!(
            SourceFingerprint::check_kiro_path_samples_only(&session_path, Some(&updated_messages),),
            Some(FingerprintStatus::Changed(_))
        ));
    }

    #[test]
    fn test_droid_fingerprint_tracks_fallback_jsonl_changes() {
        let dir = TempDir::new().unwrap();
        let settings_path = dir.path().join("session.settings.json");
        std::fs::write(&settings_path, br#"{"tokenUsage":{"inputTokens":1}}"#).unwrap();

        let base = SourceFingerprint::from_droid_path(&settings_path).unwrap();

        let jsonl_path = dir.path().join("session.jsonl");
        std::fs::write(&jsonl_path, b"Model: Claude Sonnet 4\n").unwrap();
        let with_jsonl = SourceFingerprint::from_droid_path(&settings_path).unwrap();
        assert_ne!(base, with_jsonl);

        std::fs::write(&jsonl_path, b"Model: Claude Opus 4\n").unwrap();
        let updated_jsonl = SourceFingerprint::from_droid_path(&settings_path).unwrap();
        assert_ne!(with_jsonl, updated_jsonl);
    }

    #[test]
    fn test_kimi_fingerprint_tracks_legacy_config_but_keeps_kimi_code_self_contained() {
        let dir = TempDir::new().unwrap();
        let legacy_path = dir.path().join(".kimi/sessions/group/session/wire.jsonl");
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, b"usage\n").unwrap();

        let legacy_base = SourceFingerprint::from_kimi_path(&legacy_path).unwrap();
        let legacy_config = dir.path().join(".kimi/config.json");
        std::fs::write(&legacy_config, br#"{"model":"kimi-k2"}"#).unwrap();
        let legacy_with_config = SourceFingerprint::from_kimi_path(&legacy_path).unwrap();
        assert_ne!(legacy_base, legacy_with_config);

        std::fs::write(&legacy_config, br#"{"model":"kimi-k3"}"#).unwrap();
        let legacy_updated_config = SourceFingerprint::from_kimi_path(&legacy_path).unwrap();
        assert_ne!(legacy_with_config, legacy_updated_config);

        let code_path = dir
            .path()
            .join(".kimi-code/sessions/workspace/session/agents/main/wire.jsonl");
        std::fs::create_dir_all(code_path.parent().unwrap()).unwrap();
        std::fs::write(&code_path, b"usage.record\n").unwrap();
        let code_base = SourceFingerprint::from_kimi_path(&code_path).unwrap();
        assert_eq!(
            code_base,
            SourceFingerprint::from_path_samples_only(&code_path).unwrap()
        );

        assert!(crate::sessions::kimi::kimi_config_path(&code_path).is_none());
        let unrelated_config = dir.path().join(".kimi-code/config.json");
        std::fs::write(&unrelated_config, br#"{"model":"unrelated"}"#).unwrap();
        let code_with_config = SourceFingerprint::from_kimi_path(&code_path).unwrap();
        assert_eq!(code_base, code_with_config);
    }

    #[test]
    fn test_claude_sidechain_fingerprint_tracks_nested_parent_session_changes() {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join("projects/project-one");
        let sidechain_path = project_dir
            .join("parent-session/subagents")
            .join("agent-child.jsonl");
        std::fs::create_dir_all(sidechain_path.parent().unwrap()).unwrap();
        std::fs::write(
            &sidechain_path,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"sessionId":"parent-session","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let parent_path =
            crate::sessions::claudecode::parent_session_paths_for_cache(&sidechain_path)
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(parent_path, project_dir.join("parent-session.jsonl"));
        let base =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();

        std::fs::write(&parent_path, b"parent transcript 1\n").unwrap();
        let with_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(base, with_parent);

        std::fs::write(&parent_path, b"parent transcript 2\n").unwrap();
        let updated_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(with_parent, updated_parent);
    }

    #[test]
    fn test_claude_sidechain_fingerprint_tracks_flat_parent_session_changes() {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let sidechain_path = project_dir.join("agent-child.jsonl");
        let mut sidechain = format!("{}\n", "x".repeat(4096)).repeat(65);
        sidechain.push_str(concat!(
            r#"{"type":"assistant","isSidechain":true,"sessionId":"flat-parent","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            "\n"
        ));
        std::fs::write(&sidechain_path, sidechain).unwrap();

        let parent_path =
            crate::sessions::claudecode::parent_session_paths_for_cache(&sidechain_path)
                .into_iter()
                .next()
                .unwrap();
        assert_eq!(parent_path, project_dir.join("flat-parent.jsonl"));
        let base =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();

        std::fs::write(&parent_path, b"flat parent 1\n").unwrap();
        let with_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(base, with_parent);

        std::fs::write(&parent_path, b"flat parent 2\n").unwrap();
        let updated_parent =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        assert_ne!(with_parent, updated_parent);
    }

    #[test]
    fn test_claude_sidechain_warm_check_reuses_cached_parent_dependencies() {
        let dir = TempDir::new().unwrap();
        let project_dir = dir.path().join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let sidechain_path = project_dir.join("agent-child.jsonl");
        let mut sidechain = format!("{}\n", "x".repeat(4096)).repeat(65);
        sidechain.push_str(concat!(
            r#"{"type":"assistant","isSidechain":true,"sessionId":"flat-parent","agentId":"child","timestamp":"2026-01-01T00:00:00Z","requestId":"req-1","message":{"id":"msg-1","model":"claude-sonnet-4","usage":{"input_tokens":1,"output_tokens":1}}}"#,
            "\n"
        ));
        std::fs::write(&sidechain_path, sidechain).unwrap();

        let cached =
            SourceFingerprint::from_claude_code_path_with_home(&sidechain_path, None).unwrap();
        let parent_path = project_dir.join("flat-parent.jsonl");
        assert!(cached.related_files.iter().any(|related| {
            related.suffix == "parent-session-0.jsonl"
                && related.path.to_path_buf() == parent_path
                && !related.exists
        }));
        assert!(matches!(
            SourceFingerprint::check_claude_code_path_with_home_samples_only(
                &sidechain_path,
                Some(&cached),
                None,
            ),
            Some(FingerprintStatus::Unchanged)
        ));

        std::fs::write(&parent_path, b"parent transcript\n").unwrap();
        assert!(matches!(
            SourceFingerprint::check_claude_code_path_with_home_samples_only(
                &sidechain_path,
                Some(&cached),
                None,
            ),
            Some(FingerprintStatus::Changed(_))
        ));
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_meta_sidecar_changes() {
        let dir = TempDir::new().unwrap();
        let jsonl_path = dir.path().join("agent-abc123.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

        // No meta sidecar → baseline fingerprint
        let base = SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

        // Add meta sidecar → fingerprint changes
        let meta_path = dir.path().join("agent-abc123.meta.json");
        std::fs::write(&meta_path, br#"{"agentType":"explore"}"#).unwrap();
        let with_meta =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();
        assert_ne!(
            base, with_meta,
            "Adding meta sidecar should change fingerprint"
        );

        // Update meta sidecar → fingerprint changes again
        std::fs::write(&meta_path, br#"{"agentType":"executor"}"#).unwrap();
        let updated_meta =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();
        assert_ne!(
            with_meta, updated_meta,
            "Updating meta sidecar should change fingerprint"
        );

        // Main session file (no agent- prefix) → unaffected by unrelated meta files
        let main_path = dir.path().join("session-uuid.jsonl");
        std::fs::write(&main_path, b"main-session").unwrap();
        let main_fp1 =
            SourceFingerprint::from_claude_code_path_with_home(&main_path, None).unwrap();
        // Create a meta file with the main session stem (unlikely in practice)
        let main_meta = dir.path().join("session-uuid.meta.json");
        std::fs::write(&main_meta, br#"{"agentType":"x"}"#).unwrap();
        let main_fp2 =
            SourceFingerprint::from_claude_code_path_with_home(&main_path, None).unwrap();
        assert_ne!(
            main_fp1, main_fp2,
            "Claude Code fingerprints always track .meta.json if it exists"
        );
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_cc_mirror_variant_metadata_changes() {
        let dir = TempDir::new().unwrap();
        let variant_dir = dir.path().join(".cc-mirror/kimi-code");
        let config_dir = variant_dir.join("config");
        let project_dir = config_dir.join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_path = project_dir.join("session.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

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
        let with_kimi =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

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
        let with_minimax =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, None).unwrap();

        assert_ne!(
            with_kimi, with_minimax,
            "Changing cc-mirror provider metadata should invalidate parsed Claude cache entries"
        );
    }

    #[test]
    fn test_claude_code_fingerprint_tracks_cc_mirror_custom_config_dir_metadata_changes() {
        let dir = TempDir::new().unwrap();
        let variant_dir = dir.path().join(".cc-mirror/kimi-code");
        let config_dir = dir.path().join("mirror-configs/kimi-code");
        let project_dir = config_dir.join("projects/project-one");
        std::fs::create_dir_all(&project_dir).unwrap();
        let jsonl_path = project_dir.join("session.jsonl");
        std::fs::write(&jsonl_path, b"jsonl-content").unwrap();

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
        let with_kimi =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, Some(dir.path()))
                .unwrap();

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
        let with_minimax =
            SourceFingerprint::from_claude_code_path_with_home(&jsonl_path, Some(dir.path()))
                .unwrap();

        assert_ne!(
            with_kimi, with_minimax,
            "Changing cc-mirror metadata should invalidate cache entries for custom configDir layouts"
        );
    }

    #[test]
    fn test_codex_incremental_cache_requires_newline_boundary() {
        let file = write_temp_file(b"line-1\nline-2");

        assert!(build_codex_incremental_cache(
            file.path(),
            file.as_file().metadata().unwrap().len(),
            CodexParseState::default(),
        )
        .is_none());
    }

    #[test]
    fn test_codex_prefix_matches_rejects_middle_rewrite_with_same_tail() {
        let file = write_temp_file(b"aaaa\nbbbb\ncccc\n");
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        std::fs::write(file.path(), b"aaaa\nzzzz\ncccc\nmore\n").unwrap();

        assert!(!codex_prefix_matches(file.path(), &incremental_cache));
    }

    #[test]
    fn test_codex_prefix_matches_rejects_large_unsampled_rewrite() {
        let mut original = vec![b'a'; 128 * 1024];
        original.extend_from_slice(b"\n");
        let file = write_temp_file(&original);
        let fingerprint = SourceFingerprint::from_path(file.path()).unwrap();
        let incremental_cache = build_codex_incremental_cache(
            file.path(),
            fingerprint.size,
            CodexParseState::default(),
        )
        .unwrap();

        let mut rewritten = original.clone();
        rewritten[73 * 1024] = b'z';
        rewritten.extend_from_slice(b"appended\n");
        std::fs::write(file.path(), rewritten).unwrap();

        assert!(!codex_prefix_matches(file.path(), &incremental_cache));
    }

    #[test]
    #[serial_test::serial]
    fn test_source_message_cache_round_trips_multiple_sources() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let (path_one, path_two) = write_sources(&source_dir);

        let mut cache = SourceMessageCache::default();
        cache.insert(test_entry(identity, &path_one, "session-1"));
        cache.insert(test_entry(identity, &path_two, "session-2"));

        cache.check().unwrap();
        assert!(cache_dir()
            .unwrap()
            .join("source-message-cache.sqlite3")
            .is_file());
        let loaded = SourceMessageCache::load();
        assert_eq!(loaded.entries_for_test().len(), 2);
        assert!(loaded.get(identity, &path_one).is_some());
        assert!(loaded.get(identity, &path_two).is_some());

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_source_cache_preserves_large_compact_rows_across_sources() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let (path_one, path_two) = write_sources(&source_dir);

        let mut entry_one = test_entry(identity, &path_one, "session-1");
        entry_one.messages[0].model_id = "a".repeat(20 * 1024);
        let mut entry_two = test_entry(identity, &path_two, "session-2");
        entry_two.messages[0].model_id = "b".repeat(20 * 1024);

        let mut cache = SourceMessageCache::default();
        cache.insert(entry_one);
        cache.insert(entry_two);
        cache.check().unwrap();

        let loaded = SourceMessageCache::load();
        assert!(loaded.get(identity, &path_one).is_some());
        assert!(loaded.get(identity, &path_two).is_some());

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_copilot_parser_v3_same_fingerprint_rebuilds_merged_duration() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(
            br#"{"type":"span","traceId":"trace-duplicate","spanId":"span-duplicate","name":"chat gpt-5.4-mini","startTime":[1775934260,0],"endTime":[1775934270,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"gpt-5.4-mini","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":20}}
{"type":"span","traceId":"trace-duplicate","spanId":"span-duplicate","name":"chat gpt-5.4-mini","startTime":[1775934255,0],"endTime":[1775934261,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"gpt-5.4-mini","gen_ai.usage.input_tokens":30,"gen_ai.usage.output_tokens":50}}
"#,
        );
        let current = CacheIdentity::for_client(ClientId::Copilot);
        let stale = CacheIdentity {
            namespace: current.namespace,
            parser_version: 3,
            scope: [0; 32],
        };
        let fingerprint = SourceFingerprint::from_path(source.path()).unwrap();
        // Historical row is what the pre-merge parser produced for this file,
        // with only the fixed field reverted so identity/client/key stay real.
        let mut stale_message =
            crate::sessions::copilot::parse_copilot_file(source.path()).remove(0);
        stale_message.duration_ms = Some(10_000);
        let stale_entry = CachedSourceEntry::new(
            stale,
            source.path(),
            fingerprint.clone(),
            vec![stale_message],
            Vec::new(),
            None,
        );
        let payload = bincode::options().serialize(&vec![stale_entry]).unwrap();
        let stale_key = CacheKey::new(current, source.path()).shard();
        let stale_path = shard_path(&cache_shard_dir().unwrap(), &stale_key);
        ensure_cache_dir(stale_path.parent().unwrap()).unwrap();
        let stale_envelope = CachedShardEnvelope {
            format_version: CACHE_FORMAT_VERSION,
            parser_namespace: stale.namespace.to_string(),
            parser_version: stale.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&stale_path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &stale_envelope)
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(
            SourceFingerprint::from_path(source.path()).unwrap(),
            fingerprint,
            "the parser-only invalidation must rebuild an unchanged source"
        );
        let mut cache = SourceMessageCache::load();
        assert!(
            cache.get(current, source.path()).is_none(),
            "a pre-bump row must not be served to the bumped reader"
        );
        let rebuilt_messages = crate::sessions::copilot::parse_copilot_file(source.path());
        assert_eq!(rebuilt_messages.len(), 1);
        assert_eq!(rebuilt_messages[0].timestamp, 1_775_934_255_000);
        assert_eq!(rebuilt_messages[0].duration_ms, Some(15_000));
        cache.insert(CachedSourceEntry::new(
            current,
            source.path(),
            fingerprint,
            rebuilt_messages,
            Vec::new(),
            None,
        ));

        let loaded = SourceMessageCache::load();
        assert_eq!(
            loaded.get(current, source.path()).unwrap().messages[0].duration_ms,
            Some(15_000)
        );
        restore_cache_env(prev_env);
    }

    /// A shard written before RET-CLAUDE-001 (`CachedSourceEntry` without
    /// `retained_keys`) is tagged Claude `parser_version = 1`. Its payload's
    /// binary layout no longer matches the current 8-field struct, so
    /// decoding it directly would either error out on missing trailing bytes
    /// or, in the worst case, misalign fields — the version gate in
    /// `read_shard_with_limit` has to reject it by version before ever
    /// attempting `bincode::deserialize` on the payload. This hand-rolls the
    /// pre-bump 7-field layout rather than using the current
    /// `CachedSourceEntry`, so the test actually exercises the layout change
    /// the bump exists for, not just a version-number label.
    #[test]
    #[serial_test::serial]
    fn test_claude_v1_shard_is_rejected_before_its_payload_is_decoded() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"legacy claude transcript\n");
        let claude = CacheIdentity::for_client(ClientId::Claude);
        let legacy = CacheIdentity {
            namespace: claude.namespace,
            parser_version: 1,
            scope: [0; 32],
        };
        assert_ne!(
            claude.parser_version, 1,
            "this test requires Claude to be versioned past the pre-retention layout"
        );

        #[derive(Serialize)]
        struct LegacyCachedSourceEntry {
            parser_namespace: String,
            parser_version: u32,
            path: CachedPath,
            fingerprint: SourceFingerprint,
            messages: Vec<UnifiedMessage>,
            fallback_timestamp_indices: Vec<usize>,
            codex_incremental: Option<CodexIncrementalCache>,
        }

        let legacy_entry = LegacyCachedSourceEntry {
            parser_namespace: legacy.namespace.to_string(),
            parser_version: legacy.parser_version,
            path: CachedPath::from_path(source.path()),
            fingerprint: SourceFingerprint::from_path(source.path()).unwrap(),
            messages: vec![keyed_message(
                legacy.namespace,
                "session",
                "msg_legacy:req_legacy",
            )],
            fallback_timestamp_indices: Vec::new(),
            codex_incremental: None,
        };
        let payload = bincode::options().serialize(&vec![legacy_entry]).unwrap();
        let stale_key = CacheKey::new(claude, source.path()).shard();
        let stale_path = shard_path(&cache_shard_dir().unwrap(), &stale_key);
        ensure_cache_dir(stale_path.parent().unwrap()).unwrap();
        let stale_envelope = CachedShardEnvelope {
            format_version: CACHE_FORMAT_VERSION,
            parser_namespace: legacy.namespace.to_string(),
            parser_version: legacy.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&stale_path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &stale_envelope)
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert!(
            matches!(read_shard(&stale_path, claude), ShardReadStatus::Stale),
            "a v1 shard must be rejected by the version gate before its payload — which no \
             longer matches CachedSourceEntry's field layout — is ever decoded"
        );

        let loaded = SourceMessageCache::load();
        assert!(
            loaded.get(claude, source.path()).is_none(),
            "a stale-version shard is a cache miss, not a misread entry"
        );

        restore_cache_env(prev_env);
    }

    /// RET-CLAUDE-001 bumped `CACHE_FORMAT_VERSION`, not just Claude's
    /// `parser_version`, because `retained_keys` was added to
    /// `CachedSourceEntry`, which every namespace's shard uses. A shard
    /// written by a client this branch never touches (Codex, still on
    /// `parser_version` 4) under the pre-bump format must therefore also be
    /// rejected by the format gate before its 7-field payload is ever
    /// decoded against the new 8-field struct — proving the invalidation is
    /// format-wide, not scoped to Claude's own version bump. This hand-rolls
    /// the pre-bump layout rather than reusing the current
    /// `CachedSourceEntry`, so the test fails if `CACHE_FORMAT_VERSION` is
    /// reverted to 2.
    /// Writes a format-3 shard by hand and returns its path.
    ///
    /// Hand-rolled rather than built from the current types, because the point
    /// is the old positional layout: `TokenBreakdown` gained `cache_write_1h`
    /// in format 4, so a shard serialized from today's structs would not be
    /// the bytes this is meant to read.
    fn seed_format3_shard(
        identity: CacheIdentity,
        source: &Path,
        dedup_key: &str,
        retained: &[&str],
    ) -> PathBuf {
        #[derive(Serialize)]
        struct Format3TokenBreakdown {
            input: i64,
            output: i64,
            cache_read: i64,
            cache_write: i64,
            reasoning: i64,
        }
        #[derive(Serialize)]
        struct Format3UnifiedMessage {
            client: String,
            model_id: String,
            provider_id: String,
            session_id: String,
            workspace_key: Option<String>,
            workspace_label: Option<String>,
            timestamp: i64,
            date: String,
            tokens: Format3TokenBreakdown,
            cost: f64,
            cost_source: crate::sessions::CostSource,
            duration_ms: Option<i64>,
            message_count: i32,
            agent: Option<String>,
            dedup_key: Option<String>,
            dedup_aliases: Vec<String>,
            is_turn_start: bool,
        }
        #[derive(Serialize)]
        struct Format3CachedSourceEntry {
            parser_namespace: String,
            parser_version: u32,
            path: CachedPath,
            fingerprint: SourceFingerprint,
            messages: Vec<Format3UnifiedMessage>,
            fallback_timestamp_indices: Vec<usize>,
            codex_incremental: Option<CodexIncrementalCache>,
            retained_keys: HashSet<String>,
        }

        let entry = Format3CachedSourceEntry {
            parser_namespace: identity.namespace.to_string(),
            parser_version: identity.parser_version,
            path: CachedPath::from_path(source),
            fingerprint: SourceFingerprint::from_path(source).unwrap(),
            messages: vec![Format3UnifiedMessage {
                client: identity.namespace.to_string(),
                model_id: "claude-3-5-sonnet".to_string(),
                provider_id: "anthropic".to_string(),
                session_id: "session".to_string(),
                workspace_key: None,
                workspace_label: None,
                timestamp: 1,
                date: "2026-01-01".to_string(),
                tokens: Format3TokenBreakdown {
                    input: 11,
                    output: 22,
                    cache_read: 33,
                    cache_write: 44,
                    reasoning: 0,
                },
                cost: 0.5,
                cost_source: crate::sessions::CostSource::default(),
                duration_ms: None,
                message_count: 1,
                agent: None,
                dedup_key: Some(dedup_key.to_string()),
                dedup_aliases: Vec::new(),
                is_turn_start: false,
            }],
            fallback_timestamp_indices: Vec::new(),
            codex_incremental: None,
            retained_keys: retained.iter().map(|k| (*k).to_string()).collect(),
        };

        let payload = bincode::options().serialize(&vec![entry]).unwrap();
        let path = shard_path(
            &cache_shard_dir().unwrap(),
            &CacheKey::new(identity, source).shard(),
        );
        ensure_cache_dir(path.parent().unwrap()).unwrap();
        let envelope = CachedShardEnvelope {
            format_version: 3,
            parser_namespace: identity.namespace.to_string(),
            parser_version: identity.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &envelope)
            .unwrap();
        writer.flush().unwrap();
        path
    }

    #[test]
    #[serial_test::serial]
    fn test_a_format3_claude_shard_is_migrated_rather_than_discarded() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"claude transcript\n");
        let claude = CacheIdentity::for_client(ClientId::Claude);

        let path = seed_format3_shard(claude, source.path(), "msg_old:req_old", &[]);

        match read_shard(&path, claude) {
            ShardReadStatus::Loaded(entries) => {
                assert_eq!(entries.len(), 1, "the format-3 payload must decode");
                let tokens = &entries[0].messages[0].tokens;
                assert_eq!(
                    (
                        tokens.input,
                        tokens.output,
                        tokens.cache_read,
                        tokens.cache_write
                    ),
                    (11, 22, 33, 44),
                    "every pre-existing lane must survive the layout change"
                );
                assert_eq!(
                    tokens.cache_write_1h, 0,
                    "format 3 predates the split, so the whole write reads as 5-minute"
                );
            }
            ShardReadStatus::Stale => {
                panic!("a format-3 Claude shard was rejected instead of migrated")
            }
            ShardReadStatus::Missing => panic!("the seeded shard was not found"),
            ShardReadStatus::Invalid(error) => {
                panic!("the format-3 payload failed to decode: {error}")
            }
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_a_format3_shard_keeps_the_history_only_its_copy_holds() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"compacted claude transcript\n");
        let claude = CacheIdentity::for_client(ClientId::Claude);

        seed_format3_shard(
            claude,
            source.path(),
            "msg_kept:req_kept",
            &["msg_kept:req_kept"],
        );

        let loaded = SourceMessageCache::load();
        let history = loaded.retainable_history(claude, source.path());
        assert_eq!(
            history.len(),
            1,
            "a retained turn is the only copy left once a rewrite drops it from the file, \
             so a format bump that cannot read it deletes it"
        );
        assert_eq!(history[0].dedup_key.as_deref(), Some("msg_kept:req_kept"));
        assert!(
            !history[0].input_tokens_are_cache_split,
            "a format-3 turn never recorded provenance, so migration must not invent a split"
        );

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_format3_codex_usage_metadata_is_transferred() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"codex transcript\n");
        let codex = CacheIdentity::for_client(ClientId::Codex);

        let path = seed_format3_shard(codex, source.path(), "msg_codex:req_codex", &[]);

        let cache = SourceMessageCache::load();
        assert!(
            cache.get(codex, source.path()).is_none(),
            "legacy Codex facts without verified resume are not warm hits"
        );
        let mut count = 0;
        cache
            .visit_source_metadata(codex, &mut |stored_path, metadata| {
                assert_eq!(stored_path, source.path());
                assert_eq!(metadata.message_count, 1);
                assert!(!metadata.cacheable);
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1);
        assert!(
            path.exists(),
            "verified migration preserves the original legacy shard"
        );

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_non_claude_legacy_shard_is_rejected_by_the_format_bump_before_its_payload_is_decoded() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"legacy codex transcript\n");
        let codex = CacheIdentity::for_client(ClientId::Codex);

        #[derive(Serialize)]
        struct LegacyCachedSourceEntry {
            parser_namespace: String,
            parser_version: u32,
            path: CachedPath,
            fingerprint: SourceFingerprint,
            messages: Vec<UnifiedMessage>,
            fallback_timestamp_indices: Vec<usize>,
            codex_incremental: Option<CodexIncrementalCache>,
        }

        let legacy_entry = LegacyCachedSourceEntry {
            parser_namespace: codex.namespace.to_string(),
            parser_version: codex.parser_version,
            path: CachedPath::from_path(source.path()),
            fingerprint: SourceFingerprint::from_path(source.path()).unwrap(),
            messages: vec![keyed_message(
                codex.namespace,
                "session",
                "msg_legacy_codex:req_legacy",
            )],
            fallback_timestamp_indices: Vec::new(),
            codex_incremental: None,
        };
        let payload = bincode::options().serialize(&vec![legacy_entry]).unwrap();
        let stale_key = CacheKey::new(codex, source.path()).shard();
        let stale_path = shard_path(&cache_shard_dir().unwrap(), &stale_key);
        ensure_cache_dir(stale_path.parent().unwrap()).unwrap();
        let stale_envelope = CachedShardEnvelope {
            format_version: 2,
            parser_namespace: codex.namespace.to_string(),
            parser_version: codex.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&stale_path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &stale_envelope)
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert!(
            matches!(read_shard(&stale_path, codex), ShardReadStatus::Stale),
            "a pre-bump-format Codex shard must be rejected by the format gate before its \
             payload — which no longer matches CachedSourceEntry's field layout — is ever \
             decoded, even though its namespace and parser_version still match current"
        );

        let loaded = SourceMessageCache::load();
        assert!(
            loaded.get(codex, source.path()).is_none(),
            "a stale-format shard is a cache miss, not a misread entry"
        );

        restore_cache_env(prev_env);
    }

    /// Write a shard for `source` under `identity` and hand back its path.
    fn seed_shard(
        identity: CacheIdentity,
        source: &Path,
        entries: Vec<CachedSourceEntry>,
    ) -> PathBuf {
        let payload = bincode::options().serialize(&entries).unwrap();
        let key = CacheKey::new(identity, source).shard();
        let path = shard_path(&cache_shard_dir().unwrap(), &key);
        ensure_cache_dir(path.parent().unwrap()).unwrap();
        let envelope = CachedShardEnvelope {
            format_version: CACHE_FORMAT_VERSION,
            parser_namespace: identity.namespace.to_string(),
            parser_version: identity.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &envelope)
            .unwrap();
        writer.flush().unwrap();
        path
    }

    /// A pre-bump entry is hidden from `get`, so every reader re-parses, while
    /// its messages stay reachable for retention to draw on.
    #[test]
    #[serial_test::serial]
    fn test_a_pre_bump_entry_is_hidden_from_get_but_its_history_is_not() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"claude transcript\n");
        let claude = CacheIdentity::for_client(ClientId::Claude);
        let older = CacheIdentity {
            namespace: claude.namespace,
            parser_version: claude.parser_version - 1,
            scope: [0; 32],
        };

        let entry = CachedSourceEntry::new(
            older,
            source.path(),
            SourceFingerprint::from_path(source.path()).unwrap(),
            vec![keyed_message(claude.namespace, "session", "msg_live:req")],
            Vec::new(),
            None,
        );
        let path = seed_shard(older, source.path(), vec![entry]);

        assert!(
            matches!(read_shard(&path, claude), ShardReadStatus::Loaded(_)),
            "the shard decodes even though its entries are behind"
        );
        let loaded = SourceMessageCache::load();
        assert!(
            loaded.get(claude, source.path()).is_none(),
            "a pre-bump entry is not a source this build would produce"
        );
        assert_eq!(
            loaded.retainable_history(claude, source.path()).len(),
            1,
            "its messages are still the only copy retention could draw on"
        );

        restore_cache_env(prev_env);
    }

    /// The rewrite a load queues carries forward only what the scan touched,
    /// so a path the scan never reaches is where history is lost for good.
    #[test]
    #[serial_test::serial]
    fn test_a_migrated_entry_survives_a_save_that_never_reparsed_it() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"claude transcript\n");
        let claude = CacheIdentity::for_client(ClientId::Claude);
        let older = CacheIdentity {
            namespace: claude.namespace,
            parser_version: claude.parser_version - 1,
            scope: [0; 32],
        };

        let entry = CachedSourceEntry::new(
            older,
            source.path(),
            SourceFingerprint::from_path(source.path()).unwrap(),
            vec![
                keyed_message(claude.namespace, "session", "msg_live:req_live"),
                keyed_message(claude.namespace, "session", "msg_kept:req_kept"),
            ],
            Vec::new(),
            None,
        )
        .with_retained_keys(HashSet::from(["msg_kept:req_kept".to_string()]));
        seed_shard(older, source.path(), vec![entry]);

        // A scan that never reaches this path: load, then save.
        let mut cache = SourceMessageCache::load();
        cache.check().unwrap();

        let reloaded = SourceMessageCache::load();
        assert_eq!(
            reloaded.retainable_history(claude, source.path()).len(),
            2,
            "history that only the cache holds must outlive the save"
        );
        assert!(
            reloaded.get(claude, source.path()).is_none(),
            "still behind the current version, so a reader still re-parses"
        );

        restore_cache_env(prev_env);
    }

    /// Shared body for the Grok same-fingerprint parser-invalidation tests:
    /// a shard written under an older parser version, with an unchanged
    /// source file, must be treated as stale and cold-rebuilt rather than
    /// trusted. Parameterized over the stale version so both the v1->v2 and
    /// v2->v3 (generation-scoped parent retro attribution) invalidations are
    /// covered.
    fn run_grok_parser_same_fingerprint_rebuild(stale_parser_version: u32) {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let source = source_dir.path().join("unified.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"ts\":\"2023-11-14T22:13:20Z\",\"pid\":7,\"sid\":\"child-cache\",\"msg\":\"shell.turn.inference_done\",\"ctx\":{\"loop_index\":1,\"prompt_tokens\":10,\"completion_tokens\":2}}\n",
                "{\"ts\":\"2023-11-14T22:13:21Z\",\"pid\":7,\"msg\":\"subagent completed\",\"ctx\":{\"subagent_id\":\"child-cache\",\"effective_model\":\"grok-4.7\"}}\n",
            ),
        )
        .unwrap();
        let current = CacheIdentity::for_client(ClientId::Grok);
        let stale = CacheIdentity {
            namespace: current.namespace,
            parser_version: stale_parser_version,
            scope: [0; 32],
        };
        let fingerprint = SourceFingerprint::from_grok_path(&source).unwrap();
        // Historical row is the fixture's current parse with only the fixed
        // attribution reverted, so timestamps/keys stay real.
        let mut stale_message =
            crate::sessions::grok::parse_grok_unified_log_file(&source).remove(0);
        stale_message.model_id = "grok-unknown".to_string();
        let stale_entry = CachedSourceEntry::new(
            stale,
            &source,
            fingerprint.clone(),
            vec![stale_message],
            Vec::new(),
            None,
        );
        let payload = bincode::options().serialize(&vec![stale_entry]).unwrap();
        let stale_key = CacheKey::new(current, &source).shard();
        let stale_path = shard_path(&cache_shard_dir().unwrap(), &stale_key);
        ensure_cache_dir(stale_path.parent().unwrap()).unwrap();
        let stale_envelope = CachedShardEnvelope {
            format_version: CACHE_FORMAT_VERSION,
            parser_namespace: stale.namespace.to_string(),
            parser_version: stale.parser_version,
            payload,
        };
        let mut writer = BufWriter::new(File::create(&stale_path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &stale_envelope)
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert_eq!(
            SourceFingerprint::from_grok_path(&source).unwrap(),
            fingerprint,
            "the parser-only invalidation must rebuild an unchanged source"
        );
        let mut cache = SourceMessageCache::load();
        assert!(
            cache.get(current, &source).is_none(),
            "a pre-bump row must not be served to the bumped reader"
        );
        let rebuilt_messages = crate::sessions::grok::parse_grok_unified_log_file(&source);
        assert_eq!(rebuilt_messages.len(), 1);
        assert_eq!(rebuilt_messages[0].model_id, "grok-4.7");
        cache.insert(CachedSourceEntry::new(
            current,
            &source,
            fingerprint,
            rebuilt_messages.clone(),
            Vec::new(),
            None,
        ));

        let loaded = SourceMessageCache::load();
        assert_eq!(
            loaded.get(current, &source).unwrap().messages,
            rebuilt_messages
        );
        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_grok_parser_v1_same_fingerprint_rebuilds_model_attribution() {
        run_grok_parser_same_fingerprint_rebuild(1);
    }

    #[test]
    #[serial_test::serial]
    fn test_grok_parser_v2_same_fingerprint_rebuilds_model_attribution() {
        run_grok_parser_same_fingerprint_rebuild(2);
    }

    /// Codex parser identity 5 -> 6: turn detection now also recognizes the
    /// `item_completed` `UserMessage` payload Codex 0.145+ emits instead of a
    /// `user_message` event.
    ///
    /// Every rollout a user already has is unchanged on disk, so its
    /// fingerprint is unchanged, so the only thing that can make the
    /// correction reach it is the identity bump. A test that parses a fresh
    /// file proves the parser; this proves the delivery. Without the bump the
    /// stale entry below is served as a hit and `is_turn_start` stays false
    /// forever.
    #[test]
    #[serial_test::serial]
    fn test_codex_parser_v5_same_fingerprint_rebuilds_item_completed_turns() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let source = source_dir
            .path()
            .join("rollout-2026-01-01T00-00-00-abc.jsonl");
        std::fs::write(
            &source,
            concat!(
                r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"hello"}]}}}"#,
                "\n",
                r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let current = CacheIdentity::for_client(ClientId::Codex);
        let fingerprint = SourceFingerprint::from_path(&source).unwrap();

        // What a version-5 parse of that same file produced: the usage, with
        // no turn marker.
        let mut stale_message = crate::sessions::codex::parse_codex_file(&source).remove(0);
        stale_message.is_turn_start = false;
        let mut seed = SourceMessageCache::load();
        seed.insert(entry_with_messages(current, &source, vec![stale_message]));
        seed.check().unwrap();
        set_parser_version_for_test(current, 5);

        assert_eq!(
            SourceFingerprint::from_path(&source).unwrap(),
            fingerprint,
            "the source is untouched, so only the identity can force a rebuild"
        );
        let mut cache = SourceMessageCache::load();
        assert!(
            cache.get(current, &source).is_none(),
            "a version-5 entry must not be served to a version-6 reader"
        );

        let rebuilt = crate::sessions::codex::parse_codex_file(&source);
        assert_eq!(rebuilt.len(), 1);
        assert!(
            rebuilt[0].is_turn_start,
            "the cold rebuild is what carries the correction to an existing user"
        );
        cache.insert(entry_with_messages(current, &source, rebuilt.clone()));
        cache.check().unwrap();

        let loaded = SourceMessageCache::load();
        assert_eq!(loaded.get(current, &source).unwrap().messages, rebuilt);
        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_format_one_shard_is_stale_without_hiding_format_two_namespace() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let (format_one_path, valid_path) = write_sources(&source_dir);
        let codex = CacheIdentity::for_client(ClientId::Codex);

        let mut seed = SourceMessageCache::default();
        seed.insert(test_entry(identity, &valid_path, "format-two-session"));

        let stale_key = CacheShardKey {
            namespace: codex.namespace.to_string(),
            index: CacheKey::new(codex, &format_one_path).shard().index,
        };
        let stale_path = shard_path(&cache_shard_dir().unwrap(), &stale_key);
        ensure_cache_dir(stale_path.parent().unwrap()).unwrap();
        let format_one_envelope = CachedShardEnvelope {
            format_version: 1,
            parser_namespace: codex.namespace.to_string(),
            parser_version: codex.parser_version,
            payload: b"deliberately invalid format-1 payload".to_vec(),
        };
        let mut writer = BufWriter::new(File::create(&stale_path).unwrap());
        bincode::options()
            .serialize_into(&mut writer, &format_one_envelope)
            .unwrap();
        writer.flush().unwrap();
        drop(writer);

        assert!(matches!(
            read_shard(&stale_path, codex),
            ShardReadStatus::Stale
        ));
        let mut loaded = SourceMessageCache::load();
        assert!(loaded.get(identity, &valid_path).is_some());

        assert!(matches!(
            read_shard(&stale_path, codex),
            ShardReadStatus::Stale
        ));
        assert!(SourceMessageCache::load()
            .get(identity, &valid_path)
            .is_some());

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_explicit_invalidation_unlinks_discovery_but_keeps_durable_history() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source = write_temp_file(b"still exists\n");
        let identity = CacheIdentity::for_client(ClientId::Claude);

        let mut seed = SourceMessageCache::default();
        seed.insert(test_entry(identity, source.path(), "session-1"));
        assert!(SourceMessageCache::load()
            .get(identity, source.path())
            .is_some());

        let mut cache = SourceMessageCache::load();
        cache.remove(identity, source.path());

        assert!(
            source.path().is_file(),
            "invalidation must not remove the source"
        );
        // Durable history survives explicit invalidation as canonical facts.
        assert!(SourceMessageCache::load()
            .get(identity, source.path())
            .is_some());

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_stale_invalidation_preserves_concurrently_refreshed_entry() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let path = source_dir.path().join("session.jsonl");
        let identity = CacheIdentity::for_client(ClientId::Claude);
        std::fs::write(&path, b"old\n").unwrap();

        let mut seed = SourceMessageCache::default();
        seed.insert(test_entry(identity, &path, "old-session"));

        let mut stale_invalidator = SourceMessageCache::load();
        stale_invalidator.remove(identity, &path);

        std::fs::write(&path, b"fresh-content\n").unwrap();
        let mut fresh_writer = SourceMessageCache::load();
        fresh_writer.insert(test_entry(identity, &path, "fresh-session"));


        let loaded = SourceMessageCache::load();
        assert_eq!(
            loaded.get(identity, &path).unwrap().messages[0].session_id,
            "fresh-session"
        );

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_insert_merges_concurrent_writers() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let (path_one, path_two) = write_sources(&source_dir);

            let mut writer_one = SourceMessageCache::load();
            let mut writer_two = SourceMessageCache::load();

            writer_one.insert(test_entry(identity, &path_one, "session-1"));
            writer_two.insert(test_entry(identity, &path_two, "session-2"));


            let loaded = SourceMessageCache::load();
            assert!(loaded.get(identity, &path_one).is_some());
            assert!(loaded.get(identity, &path_two).is_some());
        }

        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_insert_preserves_recreated_path_from_concurrent_writer() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("session.jsonl");
            std::fs::write(&path, b"{\"id\":\"old\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);

            let mut seed = SourceMessageCache::default();
            seed.insert(test_entry(identity, &path, "old-session"));

            let mut stale_deleter = SourceMessageCache::load();
            std::fs::remove_file(&path).unwrap();
            stale_deleter.prune_missing_files();

            std::fs::write(&path, b"{\"id\":\"fresh\"}\n").unwrap();
            let mut fresh_writer = SourceMessageCache::load();
            fresh_writer.insert(test_entry(identity, &path, "fresh-session"));


            let loaded = SourceMessageCache::load();
            let entry = loaded
                .get(identity, &path)
                .expect("recreated source cache entry should survive stale delete");
            assert_eq!(entry.messages[0].session_id, "fresh-session");
        }

        restore_cache_env(prev_env);
    }

    /// A Claude entry can hold assistant turns the live transcript no longer
    /// contains. Two writers therefore hold genuinely different histories for
    /// one path, and the last writer must union the stored history rather than
    /// retire it with a wholesale replace.
    ///
    /// The key this test rescues (`dropped`) is tagged retained in `first`, so
    /// the assertion exercises the SQLite retention merge carrying
    /// provenance across — not just the message surviving under whichever
    /// label the union happens to attach. Without this, the test would still
    /// pass even if the save-time merge dropped retained provenance
    /// entirely and always labeled an absorbed key live.
    #[test]
    #[serial_test::serial]
    fn test_insert_unions_retained_history_for_the_same_path() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let shared = keyed_message(namespace, "session", "msg_shared:req_shared");
            let dropped = keyed_message(namespace, "session", "msg_dropped:req_dropped");

            let mut first = SourceMessageCache::load();
            first.insert(
                entry_with_messages(identity, &path, vec![shared.clone(), dropped])
                    .with_retained_keys(HashSet::from(["msg_dropped:req_dropped".to_string()])),
            );

            let mut second = SourceMessageCache::load();
            second.insert(entry_with_messages(identity, &path, vec![shared]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            let keys: HashSet<&str> = entry
                .messages
                .iter()
                .filter_map(|message| message.dedup_key.as_deref())
                .collect();
            assert!(keys.contains("msg_dropped:req_dropped"));
            assert_eq!(
                entry.messages.len(),
                2,
                "the shared turn must not duplicate"
            );
            assert!(
                entry.retained_keys.contains("msg_dropped:req_dropped"),
                "the absorbed key's retained provenance must survive the union, not just the \
                 message content"
            );
        }

        restore_cache_env(prev_env);
    }

    /// Inverse of the union case above: the same key is live in one writer's
    /// entry and retained in the other's. Regardless of which side is
    /// `self` and which is `stored`, the merged label must be live — a
    /// concurrent writer that actually reparsed the key from live content is
    /// authoritative over a carried-forward retained copy of the same key.
    #[test]
    #[serial_test::serial]
    fn test_insert_prefers_live_over_retained_for_a_key_both_writers_describe() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let shared = keyed_message(namespace, "session", "msg_shared:req_shared");

            // `first` retains the shared key (a carried-forward partial);
            // `second` — the last writer — saw it live.
            let mut first = SourceMessageCache::load();
            first.insert(
                entry_with_messages(identity, &path, vec![shared.clone()])
                    .with_retained_keys(HashSet::from(["msg_shared:req_shared".to_string()])),
            );

            let mut second = SourceMessageCache::load();
            second.insert(entry_with_messages(identity, &path, vec![shared]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            assert!(
                !entry.retained_keys.contains("msg_shared:req_shared"),
                "a key the last writer saw live must not be left labeled retained, even though \
                 an earlier writer's entry retained it"
            );
        }

        restore_cache_env(prev_env);

        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let shared = keyed_message(namespace, "session", "msg_shared:req_shared");

            // Same conflict, opposite arrival order: the last writer is the
            // one that retained the key, `stored` (the earlier, on-disk
            // entry) saw it live.
            let mut first = SourceMessageCache::load();
            first.insert(entry_with_messages(identity, &path, vec![shared.clone()]));

            let mut second = SourceMessageCache::load();
            second.insert(
                entry_with_messages(identity, &path, vec![shared])
                    .with_retained_keys(HashSet::from(["msg_shared:req_shared".to_string()])),
            );

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            assert!(
                !entry.retained_keys.contains("msg_shared:req_shared"),
                "a key an earlier writer saw live must not be left labeled retained just because \
                 the last writer's own entry retained it"
            );
        }

        restore_cache_env(prev_env);
    }

    /// Two writers can straddle both an in-place transcript rewrite and an
    /// unrelated `cc-mirror/variant.json` edit (a provider switch, say)
    /// landing in between. The absorbed message's client/provider/workspace
    /// must reflect the *current* on-disk variant, not whatever `stored` was
    /// first cached with — otherwise nothing ever refreshes it, since
    /// `refresh_retained_message_context` only runs at parse time and the
    /// save-merge path is the only place that ever sees `stored`'s content
    /// again.
    #[test]
    #[serial_test::serial]
    fn test_insert_refreshes_absorbed_message_metadata_against_current_variant() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let variant_dir = temp_home.path().join(".cc-mirror").join("work-variant");
            let config_dir = variant_dir.join("config");
            let path = config_dir
                .join("projects")
                .join("-Users-example-work")
                .join("session.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"{\"id\":\"pre-rewrite\"}\n").unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                serde_json::json!({
                    "name": "work-variant",
                    "provider": "openai",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();

            let identity = CacheIdentity::for_client(ClientId::Claude);
            let absorbed_key = "msg_absorbed:req_absorbed";
            let stale_message = UnifiedMessage::new_with_dedup(
                "cc-mirror/work-variant",
                "claude-3-5-sonnet",
                "openai",
                "session-1",
                1,
                TokenBreakdown {
                    input: 1,
                    output: 2,
                    ..Default::default()
                },
                0.0,
                Some(absorbed_key.to_string()),
            );

            // Writer one caches the pre-rewrite generation, retaining the
            // turn the in-place rewrite is about to drop.
            let mut first = SourceMessageCache::load();
            first.insert(
                entry_with_messages(identity, &path, vec![stale_message])
                    .with_retained_keys(HashSet::from([absorbed_key.to_string()])),
            );

            // The rewrite lands (dropping the turn from the live file), and a
            // related-metadata edit — a variant provider switch — lands
            // alongside it.
            std::fs::write(&path, b"{\"id\":\"post-rewrite\"}\n").unwrap();
            std::fs::write(
                variant_dir.join("variant.json"),
                serde_json::json!({
                    "name": "work-variant",
                    "provider": "zai",
                    "configDir": config_dir,
                })
                .to_string(),
            )
            .unwrap();

            // Writer two's own parse of the new generation found nothing (the
            // fixture never runs the real parser); its cache entry gets
            // saved and absorbs the retained turn from `first` on merge.
            let mut second = SourceMessageCache::load();
            second.insert(entry_with_messages(identity, &path, Vec::new()));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(entry.messages.len(), 1, "the retained turn must survive");
            assert_eq!(
                entry.messages[0].provider_id, "zai",
                "an absorbed message must carry the current variant's provider, not the stale \
                 one it was first cached with"
            );

            // A following cache-hit scan serves the persisted entry as-is —
            // the refreshed metadata must already be what got saved, not
            // something a live re-parse would still need to fix up.
            let rehit = SourceMessageCache::load();
            let rehit_entry = rehit
                .get(identity, &path)
                .expect("cache hit should still see the entry");
            assert_eq!(rehit_entry.messages[0].provider_id, "zai");
        }

        restore_cache_env(prev_env);
    }

    /// Regression for the finding that clearing `retained_keys`' label
    /// without replacing `self.messages`' content left the completed
    /// replay's token counts unabsorbed: the merged entry kept the
    /// partial's own content under a live label, which a following
    /// cache-hit scan would then treat as authoritative.
    #[test]
    #[serial_test::serial]
    fn test_insert_prefers_live_message_content_not_just_the_cleared_label() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let key = "msg_shared:req_shared";
            let retained_partial = UnifiedMessage::new_with_dedup(
                namespace,
                "claude-3-5-sonnet",
                "anthropic",
                "session",
                1,
                TokenBreakdown {
                    input: 1,
                    output: 50,
                    ..Default::default()
                },
                0.0,
                Some(key.to_string()),
            );
            let completed_live = UnifiedMessage::new_with_dedup(
                namespace,
                "claude-3-5-sonnet",
                "anthropic",
                "session",
                1,
                TokenBreakdown {
                    input: 1,
                    output: 999,
                    ..Default::default()
                },
                0.0,
                Some(key.to_string()),
            );

            // `first` — the earlier, on-disk writer — saw the completed
            // replay live; `second`, the last writer being saved (`self` in
            // the merge), only holds a carried-forward retained partial for
            // the same key.
            let mut first = SourceMessageCache::load();
            first.insert(entry_with_messages(identity, &path, vec![completed_live]));

            let mut second = SourceMessageCache::load();
            second.insert(
                entry_with_messages(identity, &path, vec![retained_partial])
                    .with_retained_keys(HashSet::from([key.to_string()])),
            );

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            assert_eq!(
                entry.messages[0].tokens.output, 999,
                "the merged entry must carry the completed replay's token counts, not just a \
                 cleared retained label over the partial's own content"
            );
            assert!(!entry.retained_keys.contains(key));

            // A following cache-hit scan (a second load with no further
            // writes) must keep preferring the completed copy.
            let rescan = SourceMessageCache::load();
            let entry = rescan.get(identity, &path).expect("entry should survive");
            assert_eq!(entry.messages.len(), 1);
            assert_eq!(entry.messages[0].tokens.output, 999);
            assert!(!entry.retained_keys.contains(key));
        }

        restore_cache_env(prev_env);
    }

    /// Two writers can straddle an in-place transcript rewrite: one loads
    /// and saves the full pre-rewrite file, the other loads the freshly
    /// compacted file (whose parse never observed the turn compaction
    /// dropped, since its own cache load predated the first writer's save)
    /// and saves last. The two entries' fingerprints therefore differ, and
    /// the merge must not let that fingerprint mismatch discard the first
    /// writer's history — the dropped turn must survive, labeled retained
    /// relative to this (the second writer's) generation.
    #[test]
    #[serial_test::serial]
    fn test_insert_rescues_history_across_a_straddled_rewrite_fingerprint_mismatch() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let dropped = UnifiedMessage::new_with_dedup(
                namespace,
                "claude-3-5-sonnet",
                "anthropic",
                "session",
                1,
                TokenBreakdown {
                    input: 1,
                    output: 777,
                    ..Default::default()
                },
                0.0,
                Some("msg_dropped:req_dropped".to_string()),
            );
            let kept = keyed_message(namespace, "session", "msg_kept:req_kept");

            // Writer FULL parses the whole pre-rewrite file and saves first.
            std::fs::write(&path, b"{\"id\":\"pre-rewrite\"}\n").unwrap();
            let mut full_writer = SourceMessageCache::load();
            full_writer.insert(entry_with_messages(
                identity,
                &path,
                vec![dropped.clone(), kept.clone()],
            ));

            // Compaction changes the fingerprint and drops `dropped`'s turn.
            // Writer COMPACTED's own parse never saw it and saves last.
            std::fs::write(&path, b"{\"id\":\"compacted\"}\n").unwrap();
            let mut compacted_writer = SourceMessageCache::load();
            compacted_writer.insert(entry_with_messages(identity, &path, vec![kept]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                2,
                "the dropped turn must survive a rewrite straddled by two writers"
            );
            let rescued = entry
                .messages
                .iter()
                .find(|message| message.dedup_key.as_deref() == Some("msg_dropped:req_dropped"))
                .expect("the dropped turn's content must be absorbed, not just its key");
            assert_eq!(
                rescued.tokens.output, 777,
                "the rescued message must carry the pre-rewrite writer's own content"
            );
            assert!(
                entry.retained_keys.contains("msg_dropped:req_dropped"),
                "a key absent from this generation's own parse must be labeled retained, \
                 whatever the other generation's entry called it"
            );
        }

        restore_cache_env(prev_env);
    }

    /// Straddle case the previous test doesn't cover: `self` does not just
    /// lack the key, it already holds its *own* copy of it — a partial turn
    /// parsed before the file was rewritten out from under it — while
    /// `stored` (the earlier writer's save) holds a completed replay of the
    /// same key under a different fingerprint. Discarding either side loses
    /// data once compaction removes the turn from the live file entirely, so
    /// the merge must reconcile the two copies' token counts field-by-field,
    /// the same way the parser folds same-file streaming duplicates —
    /// neither copy alone carries every field's true maximum.
    #[test]
    #[serial_test::serial]
    fn test_insert_reconciles_token_counts_across_a_fingerprint_mismatch() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let key = "msg_shared:req_shared";

            // `completed` leads on output and cache_read; `partial` leads on
            // input. Neither field-set alone is the true maximum, so a fix
            // that just picks one whole message over the other would still
            // fail this.
            let completed = UnifiedMessage::new_with_dedup(
                namespace,
                "claude-3-5-sonnet",
                "anthropic",
                "session",
                1,
                TokenBreakdown {
                    input: 80,
                    output: 999,
                    cache_read: 10,
                    cache_write: 5,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
                Some(key.to_string()),
            );
            // The completed record carries the finished timing; the partial
            // was snapshotted mid-response and has a shorter one. Neither is
            // the maximum on every field, so picking one whole message loses
            // something either way.
            let completed = {
                let mut m = completed;
                m.duration_ms = Some(4_200);
                m
            };
            let partial = UnifiedMessage::new_with_dedup(
                namespace,
                "claude-3-5-sonnet",
                "anthropic",
                "session",
                1,
                TokenBreakdown {
                    input: 100,
                    output: 50,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                    cache_write_1h: 0,
                },
                0.0,
                Some(key.to_string()),
            );
            let partial = {
                let mut m = partial;
                m.duration_ms = Some(900);
                m
            };

            // Writer B parses the completed form and saves first, under this
            // generation's fingerprint.
            std::fs::write(&path, b"{\"id\":\"generation-b\"}\n").unwrap();
            let mut writer_b = SourceMessageCache::load();
            writer_b.insert(entry_with_messages(identity, &path, vec![completed]));

            // The file changes generation again (a straddled rewrite), and
            // writer A — which had already parsed a stalled partial of the
            // same turn under its own earlier generation — saves last.
            std::fs::write(&path, b"{\"id\":\"generation-a\"}\n").unwrap();
            let mut writer_a = SourceMessageCache::load();
            writer_a.insert(entry_with_messages(identity, &path, vec![partial]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            let merged = &entry.messages[0];
            assert_eq!(
                merged.tokens.input, 100,
                "input: partial's field was the max"
            );
            assert_eq!(
                merged.tokens.output, 999,
                "output: completed's field was the max"
            );
            assert_eq!(
                merged.duration_ms,
                Some(4_200),
                "duration: the completed timing must not be lost to the partial's shorter one"
            );
            assert_eq!(
                merged.tokens.cache_read, 10,
                "cache_read: completed's field was the max"
            );
            assert_eq!(
                merged.tokens.cache_write, 5,
                "cache_write: completed's field was the max"
            );

            // A following cache-hit scan (no further writes) must keep
            // reporting the reconciled figures, not just hold them in memory.
            let rescan = SourceMessageCache::load();
            let entry = rescan.get(identity, &path).expect("entry should survive");
            assert_eq!(entry.messages.len(), 1);
            let merged = &entry.messages[0];
            assert_eq!(merged.tokens.input, 100);
            assert_eq!(merged.tokens.output, 999);
            assert_eq!(merged.tokens.cache_read, 10);
            assert_eq!(merged.duration_ms, Some(4_200));
            assert_eq!(merged.tokens.cache_write, 5);
        }

        restore_cache_env(prev_env);
    }

    /// Matching retained labels do not imply matching content. Writers
    /// spanning more than one rewrite can land a partial and a completed form
    /// of the same turn into a single fingerprint, both labeled retained. This
    /// used to fall through untouched on the reasoning that neither side
    /// outranked the other, which discarded the completed usage exactly the
    /// way the cross-fingerprint case did.
    #[test]
    #[serial_test::serial]
    fn test_insert_reconciles_two_retained_copies_under_one_fingerprint() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("transcript.jsonl");
            // Written once and never changed, so both writers save under the
            // same fingerprint.
            std::fs::write(&path, b"{\"id\":\"one-generation\"}\n").unwrap();

            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let key = "msg_shared:req_shared";
            let retained: HashSet<String> = [key.to_string()].into_iter().collect();

            let mut completed = keyed_message(namespace, "session", key);
            completed.tokens.output = 999;
            completed.tokens.cache_read = 10;
            completed.duration_ms = Some(4_200);

            let mut partial = keyed_message(namespace, "session", key);
            partial.tokens.input = 100;
            partial.tokens.output = 50;
            partial.duration_ms = Some(900);

            let mut writer_b = SourceMessageCache::load();
            writer_b.insert(
                entry_with_messages(identity, &path, vec![completed])
                    .with_retained_keys(retained.clone()),
            );

            let mut writer_a = SourceMessageCache::load();
            writer_a.insert(
                entry_with_messages(identity, &path, vec![partial])
                    .with_retained_keys(retained.clone()),
            );

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            let merged = &entry.messages[0];
            assert_eq!(merged.tokens.input, 100, "input: partial held the max");
            assert_eq!(merged.tokens.output, 999, "output: completed held the max");
            assert_eq!(
                merged.tokens.cache_read, 10,
                "cache_read: completed held the max"
            );
            assert_eq!(
                merged.duration_ms,
                Some(4_200),
                "duration: the completed timing must survive"
            );
            assert!(
                entry.retained_keys.contains(key),
                "both copies were retained, so the merged one stays retained"
            );
        }
        restore_cache_env(prev_env);
    }

    /// Two scanners can fingerprint the same partial file and have the
    /// response complete between their parses, so two *live* copies can
    /// disagree under one fingerprint too. Preferring the stored one because
    /// it is live discards the completed usage whenever the completed writer
    /// saved last.
    #[test]
    #[serial_test::serial]
    fn test_insert_reconciles_two_live_copies_under_one_fingerprint() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("transcript.jsonl");
            std::fs::write(&path, b"{\"id\":\"one-generation\"}\n").unwrap();

            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let key = "msg_shared:req_shared";

            // Neither writer labels the key retained: both parsed it live.
            let mut partial = keyed_message(namespace, "session", key);
            partial.tokens.input = 100;
            partial.tokens.output = 50;
            partial.duration_ms = Some(900);

            let mut completed = keyed_message(namespace, "session", key);
            completed.tokens.output = 999;
            completed.tokens.cache_read = 10;
            completed.duration_ms = Some(4_200);
            completed.cost = 0.25;
            // The full generation still had the preceding user row, so it saw
            // this reply as a turn start; the compacted parse does not.
            completed.is_turn_start = true;

            // The completed writer saves first, so its copy is the one already
            // on disk; the partial writer saves last and its entry is the one
            // doing the merging. This is the losing direction: whatever the
            // merge fails to pull across from the stored copy is gone.
            let mut writer_completed = SourceMessageCache::load();
            writer_completed.insert(entry_with_messages(identity, &path, vec![completed]));

            let mut writer_partial = SourceMessageCache::load();
            writer_partial.insert(entry_with_messages(identity, &path, vec![partial]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(
                entry.messages.len(),
                1,
                "the shared turn must not duplicate"
            );
            let merged = &entry.messages[0];
            assert_eq!(merged.tokens.input, 100, "input: partial held the max");
            assert_eq!(merged.tokens.output, 999, "output: completed held the max");
            assert_eq!(
                merged.tokens.cache_read, 10,
                "cache_read: completed held the max"
            );
            assert_eq!(
                merged.duration_ms,
                Some(4_200),
                "duration: the completed timing must survive"
            );
            assert!(
                merged.is_turn_start,
                "turn start: a marker either side asserted must survive, or turn_count silently drops it"
            );
            assert_eq!(
                merged.cost, 0.25,
                "cost must track the reconciled tokens, not the copy this entry started from"
            );
            assert!(
                !entry.retained_keys.contains(key),
                "neither copy was retained, so the merged one is not either"
            );
        }
        restore_cache_env(prev_env);
    }

    /// A Claude tool-result key embeds the transcript stem. It cannot collapse
    /// against a replay under a forked filename, so save-time retention must
    /// leave it behind even though assistant history is unioned.
    #[test]
    #[serial_test::serial]
    fn test_insert_does_not_union_path_scoped_keys() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("conversation.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Claude);
            let namespace = ClientId::Claude.as_str();
            let shared = keyed_message(namespace, "session", "msg_shared:req_shared");
            let tool_result = keyed_message(
                namespace,
                "session",
                "claude:tool_result:conversation:tool_result:toolu_1",
            );

            let mut first = SourceMessageCache::load();
            first.insert(entry_with_messages(
                identity,
                &path,
                vec![shared.clone(), tool_result],
            ));

            let mut second = SourceMessageCache::load();
            second.insert(entry_with_messages(identity, &path, vec![shared]));

            let loaded = SourceMessageCache::load();
            let entry = loaded.get(identity, &path).expect("entry should survive");
            assert_eq!(entry.messages.len(), 1);
        }

        restore_cache_env(prev_env);
    }

    /// Namespaces stay isolated: a Claude insert for a shared path must not
    /// disturb the Codex entry for that same path.
    #[test]
    #[serial_test::serial]
    fn test_insert_still_replaces_entries_for_non_retaining_clients() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());

        {
            let source_dir = TempDir::new().unwrap();
            let path = source_dir.path().join("rollout.jsonl");
            std::fs::write(&path, b"{\"id\":\"live\"}\n").unwrap();
            let identity = CacheIdentity::for_client(ClientId::Codex);
            let namespace = ClientId::Codex.as_str();

            let mut first = SourceMessageCache::load();
            first.insert(entry_with_messages(
                identity,
                &path,
                vec![keyed_message(namespace, "session", "codex-key-a")],
            ));

            let claude = CacheIdentity::for_client(ClientId::Claude);
            let mut isolated = SourceMessageCache::load();
            isolated.insert(entry_with_messages(
                claude,
                &path,
                vec![keyed_message(claude.namespace, "session", "claude-current")],
            ));
            isolated.check().unwrap();
            assert_eq!(isolated.get(claude, &path).unwrap().messages.len(), 1);
            assert_eq!(
                isolated.get(identity, &path).unwrap().messages[0]
                    .dedup_key
                    .as_deref(),
                Some("codex-key-a")
            );
        }

        restore_cache_env(prev_env);
    }

    #[test]
    fn test_cache_key_namespace_prevents_path_collision() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("same-source.jsonl");
        std::fs::write(&path, b"same\n").unwrap();
        let claude = CacheKey::new(CacheIdentity::for_client(ClientId::Claude), &path);
        let codex = CacheKey::new(CacheIdentity::for_client(ClientId::Codex), &path);
        assert_ne!(claude, codex);
        assert_ne!(claude.namespace, codex.namespace);
    }

    #[test]
    #[serial_test::serial]
    fn test_legacy_monolith_is_inert_and_untouched() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let legacy = cache_dir().unwrap().join("source-message-cache.bin");
        ensure_cache_dir(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"legacy sentinel bytes\n").unwrap();
        let before = std::fs::metadata(&legacy).unwrap();
        let bytes = std::fs::read(&legacy).unwrap();

        let mut cache = SourceMessageCache::load();
        let source = write_temp_file(b"cold-build\n");
        cache.insert(test_entry(
            CacheIdentity::for_client(ClientId::Claude),
            source.path(),
            "cold",
        ));

        assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        let after = std::fs::metadata(&legacy).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());
        assert!(cache_dir()
            .unwrap()
            .join("source-message-cache.sqlite3")
            .is_file());
        restore_cache_env(prev_env);
    }

    #[test]
    #[serial_test::serial]
    fn test_changing_one_source_leaves_other_generations_unchanged() {
        let temp_home = TempDir::new().unwrap();
        let prev_env = sandbox_cache_env(temp_home.path());
        let source_dir = TempDir::new().unwrap();
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let (first, second) = write_sources(&source_dir);
        let mut cache = SourceMessageCache::default();
        cache.insert(test_entry(identity, &first, "first"));
        cache.insert(test_entry(identity, &second, "second"));
        let second_generation = cache
            .metadata(identity, &second)
            .unwrap()
            .unwrap()
            .generation;
        cache.insert(test_entry(identity, &first, "first-updated"));
        cache.check().unwrap();
        assert_eq!(
            cache
                .metadata(identity, &second)
                .unwrap()
                .unwrap()
                .generation,
            second_generation
        );
        assert_eq!(
            cache.get(identity, &first).unwrap().messages[0].session_id,
            "first-updated"
        );
        restore_cache_env(prev_env);
    }

    #[cfg(unix)]
    #[test]
    fn test_cached_path_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![0x66, 0x6f, 0x80, 0x6f]));
        let cached_path = CachedPath::from_path(&path);

        assert_eq!(cached_path.to_path_buf(), path);
    }
}
