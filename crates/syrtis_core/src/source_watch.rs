//! Native source hints are advisory; complete scanner discovery remains authoritative.

use crate::message_cache::{
    hash_bytes, sample_offsets, CachedPath, FingerprintStatus, SourceFingerprint,
};
use crate::{scanner, ClientId, LocalParseOptions, ResolvedLocalSourceContext};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Clone, PartialEq, Eq)]
pub struct LocalSourceWatchRoot {
    pub source_root: PathBuf,
    pub watch_root: PathBuf,
    pub is_directory: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalSourceOwnedWriteRule {
    pub parent: PathBuf,
    pub pattern: String,
}

/// Owns roots and a private file-backed metadata table on the canonical store.
/// Known checks never discover children/indirections or decode message bodies.
/// Periodic complete discovery is mandatory, even if every check is clean.
pub struct LocalSourceWatchManifest {
    roots: Vec<LocalSourceWatchRoot>,
    ignored: Vec<PathBuf>,
    ignored_rules: Vec<LocalSourceOwnedWriteRule>,
    known: Mutex<KnownStore>,
}

/// Targeted-change plan. `Unchanged` reuses the old manifest, `Changed`
/// carries only same-identity regular-file changes plus a replacement holding
/// exactly the pre-ingestion observations, `Rediscover` falls back to
/// authoritative whole ingestion. Never recursive discovery.
pub enum LocalSourceVerification {
    Unchanged,
    Changed {
        paths: Vec<PathBuf>,
        manifest: Arc<LocalSourceWatchManifest>,
    },
    Rediscover,
}

/// Maximum eligible file changes in one targeted plan; overflow rediscovers.
const MAX_CHANGED_PATHS: usize = 256;

impl LocalSourceWatchManifest {
    pub fn watch_roots(&self) -> &[LocalSourceWatchRoot] {
        &self.roots
    }
    pub fn ignored_paths(&self) -> &[PathBuf] {
        &self.ignored
    }
    pub fn ignored_basename_rules(&self) -> &[LocalSourceOwnedWriteRule] {
        &self.ignored_rules
    }

    /// True is dirty, false is verified unchanged; unreadability is an error,
    /// never absence. A dirty result does not mutate the immutable baseline.
    pub fn check_known_sources(&self) -> Result<bool, String> {
        let store = self
            .known
            .lock()
            .map_err(|_| "source manifest lock unavailable")?;
        store.admit()?;
        let mut statement = store
            .db
            .prepare("SELECT path,kind,state FROM temp.watch_known ORDER BY path")
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut rows = statement
            .query([])
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut changed = false;
        while let Some(row) = rows
            .next()
            .map_err(|_| "source manifest metadata read failed")?
        {
            let path = row
                .get_ref(0)
                .map_err(|_| "source manifest path read failed")?;
            let path: CachedPath =
                bincode::deserialize(path.as_blob().map_err(|_| "source manifest path invalid")?)
                    .map_err(|_| "source manifest path invalid")?;
            let kind: i64 = row.get(1).map_err(|_| "source manifest kind read failed")?;
            let state = row
                .get_ref(2)
                .map_err(|_| "source manifest fingerprint read failed")?;
            let state: Option<ObservedFile> = bincode::deserialize(
                state
                    .as_blob()
                    .map_err(|_| "source manifest fingerprint invalid")?,
            )
            .map_err(|_| "source manifest fingerprint invalid")?;
            changed |= observe(&path.into_path_buf(), ProbeKind::from_tag(kind)?)? != state;
        }
        drop(rows);
        drop(statement);
        store.admit()?;
        if let Some(revision) = store.retained_revision {
            changed |= KnownStore::retained_revision(&store.db)? != revision;
        }
        Ok(changed)
    }

    /// Full authoritative metadata check with a targeted-change plan. Every
    /// known probe is observed, store admission is verified before and after,
    /// and retained-history drift forces rediscovery. Only existing
    /// same-identity regular-file changes are eligible; anything else dirty
    /// (topology, absent/created/deleted/replaced, database/WAL) rediscovers.
    /// `Unchanged` creates no replacement store and reuses this baseline. `Changed`
    /// carries a replacement holding exactly these pre-ingestion observations,
    /// so late writes after this read still dirty the replacement.
    pub fn verify_changes(&self) -> Result<LocalSourceVerification, String> {
        let store = self
            .known
            .lock()
            .map_err(|_| "source manifest lock unavailable")?;
        store.admit()?;
        let mut statement = store
            .db
            .prepare("SELECT path,kind,state FROM temp.watch_known ORDER BY path")
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut rows = statement
            .query([])
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut fresh: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        let mut rediscover = false;
        while let Some(row) = rows
            .next()
            .map_err(|_| "source manifest metadata read failed")?
        {
            let path = row
                .get_ref(0)
                .map_err(|_| "source manifest path read failed")?;
            let path: CachedPath =
                bincode::deserialize(path.as_blob().map_err(|_| "source manifest path invalid")?)
                    .map_err(|_| "source manifest path invalid")?;
            let kind: i64 = row.get(1).map_err(|_| "source manifest kind read failed")?;
            let kind = ProbeKind::from_tag(kind)?;
            let state = row
                .get_ref(2)
                .map_err(|_| "source manifest fingerprint read failed")?;
            let state: Option<ObservedFile> = bincode::deserialize(
                state
                    .as_blob()
                    .map_err(|_| "source manifest fingerprint invalid")?,
            )
            .map_err(|_| "source manifest fingerprint invalid")?;
            let path = path.into_path_buf();
            let current = observe(&path, kind)?;
            if current == state {
                continue;
            }
            let eligible = kind == ProbeKind::File
                && matches!((&state, &current), (Some(old), Some(now)) if old.identity == now.identity);
            if !eligible {
                rediscover = true;
                continue;
            }
            // ponytail: cap resident changed paths at 256; overflow rediscovers.
            if fresh.len() < MAX_CHANGED_PATHS {
                fresh.insert(
                    path,
                    bincode::serialize(&current)
                        .map_err(|_| "source manifest fingerprint encoding failed")?,
                );
            } else {
                rediscover = true;
            }
        }
        drop(rows);
        drop(statement);
        store.admit()?;
        if let Some(revision) = store.retained_revision {
            rediscover |= KnownStore::retained_revision(&store.db)? != revision;
        }
        if rediscover {
            return Ok(LocalSourceVerification::Rediscover);
        }
        if fresh.is_empty() {
            return Ok(LocalSourceVerification::Unchanged);
        }
        let manifest = Arc::new(LocalSourceWatchManifest {
            roots: self.roots.clone(),
            ignored: self.ignored.clone(),
            ignored_rules: self.ignored_rules.clone(),
            known: Mutex::new(store.fork(&fresh)?),
        });
        Ok(LocalSourceVerification::Changed {
            paths: fresh.into_keys().collect(),
            manifest,
        })
    }

    /// Targeted hint verification. Rust parses every file URL, validates each
    /// hint against this captured baseline, and observes only the named rows.
    /// Hints are never trusted as changed evidence: every returned path is
    /// freshly observed with its parser dependency siblings (Claude
    /// `.meta.json`/mirror variants/parent sessions, Jcode journals, Roo
    /// histories, Droid/Kimi/Kiro/Grok siblings). Unknown, empty, over-cap,
    /// malformed, non-file, absent, created, deleted, replaced, directory,
    /// database, WAL, dependency-drift, or retained-revision-drift input
    /// returns `Rediscover`, never a narrowed plan; empty input keeps the
    /// authoritative `verify_changes`. `Unchanged` is never returned: only a
    /// full scan may claim nothing changed. `Changed` carries a replacement
    /// holding exactly these pre-ingestion observations, so late writes after
    /// this read still dirty the replacement.
    pub fn verify_hinted_changes(
        &self,
        home_dir: &Path,
        hints: &[String],
    ) -> Result<LocalSourceVerification, String> {
        if hints.is_empty() || hints.len() > MAX_CHANGED_PATHS {
            return self.verify_changes();
        }
        // Decode with the same URL parser as the FFI file_url emitter, so a
        // hint that cannot round-trip an emitted watch URL falls back.
        let mut candidates = Vec::with_capacity(hints.len());
        for hint in hints {
            let url = reqwest::Url::parse(hint).map_err(|_| "hint unavailable")?;
            if url.scheme() != "file"
                || url.host_str().is_some_and(|host| !host.is_empty())
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Ok(LocalSourceVerification::Rediscover);
            }
            candidates.push(url.to_file_path().map_err(|_| "hint unavailable")?);
        }
        let store = self
            .known
            .lock()
            .map_err(|_| "source manifest lock unavailable")?;
        store.admit()?;
        // Avoid materializing every known probe for a bounded hint batch.
        let mut statement = store
            .db
            .prepare("SELECT kind,state FROM temp.watch_known WHERE path=?1")
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut baseline =
            |path: &Path| -> Result<Option<(ProbeKind, Option<ObservedFile>)>, String> {
                let key = bincode::serialize(&CachedPath::from_path(path))
                    .map_err(|_| "source manifest path encoding failed")?;
                let row: Option<(i64, Vec<u8>)> = statement
                    .query_row([key], |row| Ok((row.get(0)?, row.get(1)?)))
                    .optional()
                    .map_err(|_| "source manifest metadata read failed")?;
                row.map(|(kind, state)| {
                    Ok((
                        ProbeKind::from_tag(kind)?,
                        bincode::deserialize(&state)
                            .map_err(|_| "source manifest fingerprint invalid")?,
                    ))
                })
                .transpose()
            };
        if let Some(revision) = store.retained_revision {
            if KnownStore::retained_revision(&store.db)? != revision {
                return Ok(LocalSourceVerification::Rediscover);
            }
        }
        let mut fresh: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
        // ponytail: byte-exact known-row match; alias spellings rediscover.
        for candidate in &candidates {
            let Some((kind, state)) = baseline(candidate)? else {
                return Ok(LocalSourceVerification::Rediscover);
            };
            if kind != ProbeKind::File {
                return Ok(LocalSourceVerification::Rediscover);
            }
            // Parser siblings live in their own known rows; a hint naming only
            // the primary must still verify each derivable sibling of the
            // CURRENT path. A new sibling appearing (or an old one
            // changing/disappearing) is observed here and forces rediscovery.
            // Derivations that are not baseline rows belong to other clients
            // and are ignored; a coincidental match only rediscovers.
            for sibling in hint_sibling_paths(candidate, home_dir) {
                if sibling == *candidate {
                    continue;
                }
                let Some((sibling_kind, sibling_state)) = baseline(&sibling)? else {
                    continue;
                };
                if observe(&sibling, sibling_kind)? != sibling_state {
                    return Ok(LocalSourceVerification::Rediscover);
                }
            }
            let current = observe(candidate, kind)?;
            let eligible = matches!((&state, &current), (Some(old), Some(now)) if old.identity == now.identity);
            if !eligible || current == state {
                // Unchanged, absent/created/deleted, replaced, or unreadable:
                // the hint proves nothing beyond this row, so fall back to the
                // authoritative scan, which still discovers unhinted changes.
                return Ok(LocalSourceVerification::Rediscover);
            }
            // ponytail: hints are pre-capped at 256, so this map cannot overflow.
            fresh.insert(
                candidate.clone(),
                bincode::serialize(&current)
                    .map_err(|_| "source manifest fingerprint encoding failed")?,
            );
        }
        drop(statement);
        store.admit()?;
        if let Some(revision) = store.retained_revision {
            if KnownStore::retained_revision(&store.db)? != revision {
                return Ok(LocalSourceVerification::Rediscover);
            }
        }
        let manifest = Arc::new(LocalSourceWatchManifest {
            roots: self.roots.clone(),
            ignored: self.ignored.clone(),
            ignored_rules: self.ignored_rules.clone(),
            known: Mutex::new(store.fork(&fresh)?),
        });
        Ok(LocalSourceVerification::Changed {
            paths: fresh.into_keys().collect(),
            manifest,
        })
    }

    /// Includes spare native capacities, SQLite-owned cache/schema/statement
    /// memory and a conservative connection/allocator reserve. Historical
    /// dependencies stay in SQLite TEMP files, not in the resident manifest.
    pub fn retained_bytes(&self) -> usize {
        fn path_bytes(path: &PathBuf) -> usize {
            path.capacity()
                .saturating_mul(if cfg!(windows) { 2 } else { 1 })
                .saturating_add(32)
        }
        let mut bytes = std::mem::size_of::<Self>()
            .saturating_add(
                self.roots
                    .capacity()
                    .saturating_mul(std::mem::size_of::<LocalSourceWatchRoot>()),
            )
            .saturating_add(
                self.ignored
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PathBuf>()),
            )
            .saturating_add(
                self.ignored_rules
                    .capacity()
                    .saturating_mul(std::mem::size_of::<LocalSourceOwnedWriteRule>()),
            )
            .saturating_add(256 * 1024);
        for root in &self.roots {
            bytes = bytes
                .saturating_add(path_bytes(&root.source_root))
                .saturating_add(path_bytes(&root.watch_root));
        }
        for path in &self.ignored {
            bytes = bytes.saturating_add(path_bytes(path));
        }
        for rule in &self.ignored_rules {
            bytes = bytes
                .saturating_add(path_bytes(&rule.parent))
                .saturating_add(rule.pattern.capacity())
                .saturating_add(32);
        }
        if let Ok(store) = self.known.lock() {
            bytes = bytes.saturating_add(path_bytes(&store.path));
            bytes = bytes
                .saturating_add(store.epoch.capacity())
                .saturating_add(32);
            for op in [
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED,
                rusqlite::ffi::SQLITE_DBSTATUS_SCHEMA_USED,
                rusqlite::ffi::SQLITE_DBSTATUS_STMT_USED,
            ] {
                let (mut current, mut high) = (0, 0);
                // SAFETY: the mutex exclusively owns this live SQLite handle.
                if unsafe {
                    rusqlite::ffi::sqlite3_db_status(
                        store.db.handle(),
                        op,
                        &mut current,
                        &mut high,
                        0,
                    )
                } == rusqlite::ffi::SQLITE_OK
                {
                    bytes = bytes.saturating_add(current.max(0) as usize);
                } else {
                    return usize::MAX;
                }
            }
        } else {
            return usize::MAX;
        }
        bytes
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    Directory,
    File,
    Database,
    Wal,
}

impl ProbeKind {
    fn from_tag(tag: i64) -> Result<Self, String> {
        match tag {
            0 => Ok(Self::Directory),
            1 => Ok(Self::File),
            2 => Ok(Self::Database),
            3 => Ok(Self::Wal),
            _ => Err("source manifest probe kind invalid".into()),
        }
    }
}

struct KnownStore {
    db: Connection,
    path: PathBuf,
    epoch: Vec<u8>,
    retained_revision: Option<i64>,
    #[cfg(windows)]
    file_identity: WindowsFileIdentity,
}

impl KnownStore {
    fn open(context: &ResolvedLocalSourceContext, include_retained: bool) -> Result<Self, String> {
        let root = context.usage_dir();
        crate::usage_migration::prepare(root, context.legacy_usage_roots())?;
        let mut db = crate::usage_store::open_database(root)?;
        if Self::epoch(&db)?.is_none() {
            drop(db);
            // Reuse the canonical scope schema/epoch initializer, without
            // ingesting sources or retaining its temporary query state.
            drop(crate::store_query::QueryState::open(root)?);
            db = crate::usage_store::open_database(root)?;
        }
        db.execute_batch("PRAGMA main.cache_size=-64; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-64;
            CREATE TEMP TABLE watch_known(path BLOB PRIMARY KEY,kind INTEGER NOT NULL,state BLOB NOT NULL) WITHOUT ROWID;")
            .map_err(|_| "source manifest metadata initialization failed")?;
        let epoch = Self::epoch(&db)?.ok_or("source manifest store epoch absent")?;
        let retained_revision = include_retained
            .then(|| Self::retained_revision(&db))
            .transpose()?;
        #[cfg(windows)]
        let file_identity = {
            let mut handle = std::ptr::null_mut();
            // SAFETY: SQLite writes its borrowed Win32 HANDLE; we never close it.
            let status = unsafe {
                rusqlite::ffi::sqlite3_file_control(
                    db.handle(),
                    c"main".as_ptr(),
                    rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                    (&mut handle as *mut *mut std::ffi::c_void).cast(),
                )
            };
            if status != rusqlite::ffi::SQLITE_OK {
                return Err("source manifest Windows handle unavailable".into());
            }
            windows_file_identity(handle)?
        };
        Ok(Self {
            db,
            path: root.join("source-message-cache.sqlite3"),
            epoch,
            retained_revision,
            #[cfg(windows)]
            file_identity,
        })
    }

    fn epoch(db: &Connection) -> Result<Option<Vec<u8>>, String> {
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sq_meta')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "source manifest store admission failed")?;
        if !exists {
            return Ok(None);
        }
        let epoch: Option<Vec<u8>> = db
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| "source manifest store epoch unreadable")?;
        if epoch.as_ref().is_some_and(|value| value.len() != 16) {
            return Err("source manifest store epoch invalid".into());
        }
        Ok(epoch)
    }

    fn retained_revision(db: &Connection) -> Result<i64, String> {
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='mc_meta')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "source retained-history admission failed")?;
        if !exists {
            return Ok(0);
        }
        db.query_row(
            "SELECT value FROM mc_meta WHERE key='retained_revision'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "source retained-history revision unreadable".into())
    }

    fn admit(&self) -> Result<(), String> {
        if metadata(&self.path)?.is_none() {
            return Err("source manifest store disappeared".into());
        }
        let mut moved = 0;
        // SAFETY: the connection owns this canonical inode; no ordinary file
        // descriptor is opened or closed during store admission.
        let status = unsafe {
            rusqlite::ffi::sqlite3_file_control(
                self.db.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut i32).cast(),
            )
        };
        if moved != 0
            || (status != rusqlite::ffi::SQLITE_OK && status != rusqlite::ffi::SQLITE_NOTFOUND)
        {
            return Err("source manifest store identity changed".into());
        }
        #[cfg(windows)]
        {
            let current = VfsFile::open(&self.path, false)?;
            if current.windows_identity()? != self.file_identity {
                return Err("source manifest store identity changed".into());
            }
        }
        if Self::epoch(&self.db)?.as_ref() != Some(&self.epoch) {
            return Err("source manifest store epoch changed".into());
        }
        Ok(())
    }

    fn insert(&self, path: &Path, kind: ProbeKind, required: bool) -> Result<(), String> {
        let state = observe(path, kind)?;
        if required && state.is_none() {
            return Err("discovered source disappeared before manifest capture".into());
        }
        let path = bincode::serialize(&CachedPath::from_path(path))
            .map_err(|_| "source manifest path encoding failed")?;
        let state = bincode::serialize(&state)
            .map_err(|_| "source manifest fingerprint encoding failed")?;
        self.db
            .execute(
                "INSERT OR REPLACE INTO temp.watch_known VALUES(?1,?2,?3)",
                rusqlite::params![path, kind as i64, state],
            )
            .map_err(|_| "source manifest fingerprint staging failed")?;
        Ok(())
    }

    /// Fresh disk-backed TEMP store bound to the same canonical database and
    /// epoch as `source`. Reuses the small page cache; no rescanner runs here.
    fn open_raw(root: &Path, source: &KnownStore) -> Result<Self, String> {
        let db = crate::usage_store::open_database(root)?;
        db.execute_batch("PRAGMA main.cache_size=-64; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-64;
            CREATE TEMP TABLE watch_known(path BLOB PRIMARY KEY,kind INTEGER NOT NULL,state BLOB NOT NULL) WITHOUT ROWID;")
            .map_err(|_| "source manifest metadata initialization failed")?;
        let epoch = Self::epoch(&db)?.ok_or("source manifest store epoch absent")?;
        if epoch != source.epoch {
            return Err("source manifest store epoch changed".into());
        }
        Ok(Self {
            db,
            path: source.path.clone(),
            epoch,
            retained_revision: source.retained_revision,
            #[cfg(windows)]
            file_identity: WindowsFileIdentity {
                volume: source.file_identity.volume,
                file_id: source.file_identity.file_id,
            },
        })
    }

    /// Replacement manifest store after a Changed plan. New disk-backed TEMP
    /// table holds the full pre-ingestion baseline; only eligible changes are
    /// refreshed to these observations. Never re-observes, so late writes
    /// after the read stay dirty on the replacement. Roots and exclusions are
    /// cloned by the caller; the old baseline is never mutated.
    fn fork(&self, fresh: &BTreeMap<PathBuf, Vec<u8>>) -> Result<KnownStore, String> {
        let root = self
            .path
            .parent()
            .ok_or("source manifest store root absent")?;
        let forked = KnownStore::open_raw(root, self)?;
        forked
            .db
            .execute_batch("BEGIN")
            .map_err(|_| "source manifest staging failed")?;
        let mut source = self
            .db
            .prepare("SELECT path,kind,state FROM temp.watch_known")
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut rows = source
            .query([])
            .map_err(|_| "source manifest metadata unavailable")?;
        let mut insert = forked
            .db
            .prepare("INSERT INTO temp.watch_known VALUES(?1,?2,?3)")
            .map_err(|_| "source manifest fingerprint staging failed")?;
        while let Some(row) = rows
            .next()
            .map_err(|_| "source manifest metadata read failed")?
        {
            let path = row
                .get_ref(0)
                .map_err(|_| "source manifest path read failed")?;
            let path = path.as_blob().map_err(|_| "source manifest path invalid")?;
            let kind: i64 = row.get(1).map_err(|_| "source manifest kind read failed")?;
            let state = row
                .get_ref(2)
                .map_err(|_| "source manifest fingerprint read failed")?;
            let state = state
                .as_blob()
                .map_err(|_| "source manifest fingerprint invalid")?;
            let decoded: CachedPath =
                bincode::deserialize(path).map_err(|_| "source manifest path invalid")?;
            let state = fresh
                .get(&decoded.into_path_buf())
                .map_or(state, Vec::as_slice);
            insert
                .execute(rusqlite::params![path, kind, state])
                .map_err(|_| "source manifest fingerprint staging failed")?;
        }
        drop(rows);
        drop(source);
        drop(insert);
        forked
            .db
            .execute_batch("COMMIT")
            .map_err(|_| "source manifest publication failed")?;
        forked.admit()?;
        Ok(forked)
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct ObservedFile {
    size: u64,
    modified: Option<SystemTime>,
    identity: (u64, u64),
    samples: Vec<u64>,
}

pub fn capture_local_source_watch_manifest(
    context: &ResolvedLocalSourceContext,
    options: &LocalParseOptions,
) -> Result<LocalSourceWatchManifest, String> {
    // Same default producing-client scope as local source synchronization;
    // filters and recent cutoffs do not shrink ownership/recovery coverage.
    let clients = options.clients.clone().unwrap_or_else(|| {
        let mut clients: Vec<String> = ClientId::iter()
            .filter(|id| id.parse_local())
            .map(|id| id.as_str().to_owned())
            .collect();
        clients.push("synthetic".to_owned());
        clients
    });
    let (mut scan, discovery) = scanner::scan_with_watch_discovery(context, &clients)?;
    let ignored = owned_outputs(context);
    let ignored_rules = owned_write_rules(context);
    let mut paths = discovery.roots;
    for path in discovery.topology_files {
        paths.insert(path, false);
    }
    if let Some(database) = &scan.copilot_desktop_db {
        paths.insert(
            database
                .parent()
                .ok_or("Copilot source parent unavailable")?
                .join("session-state"),
            true,
        );
    }
    let include_retained = clients.is_empty()
        || clients
            .iter()
            .any(|client| client == "claude" || client == "synthetic");
    let store = KnownStore::open(context, include_retained)?;
    store
        .db
        .execute_batch("BEGIN")
        .map_err(|_| "source manifest staging failed")?;
    let mut roots = BTreeMap::new();
    for (path, directory) in paths {
        if ignored.contains(&path) || !context.admits_scan_root(&path) {
            continue;
        }
        let directory = directory && !metadata(&path)?.is_some_and(|value| value.is_file());
        let kind = if directory {
            ProbeKind::Directory
        } else if database_filename(&path) {
            ProbeKind::Database
        } else {
            ProbeKind::File
        };
        store.insert(&path, kind, false)?;
        if kind == ProbeKind::Database {
            let wal = suffix(&path, "-wal");
            store.insert(&wal, ProbeKind::Wal, false)?;
            add_watch_target(context, &ignored, &mut roots, &wal, false)?;
        }
        add_watch_target(context, &ignored, &mut roots, &path, directory)?;
    }
    // A directory task already owns every descendant file and WAL. Retaining
    // one watch root per historical transcript would defeat the bounded lease.
    let directories: Vec<PathBuf> = roots
        .iter()
        .filter(|(_, directory)| **directory)
        .map(|(path, _)| path.clone())
        .collect();
    roots.retain(|path, _| {
        !directories
            .iter()
            .any(|root| path != root && path.starts_with(root))
    });
    let add = |path: &Path,
               kind: ProbeKind,
               required: bool,
               roots: &mut BTreeMap<PathBuf, bool>|
     -> Result<(), String> {
        if ignored.contains(&path.to_path_buf()) || !context.admits_scan_root(path) {
            return Ok(());
        }
        store.insert(path, kind, required)?;
        add_watch_target(context, &ignored, roots, path, kind == ProbeKind::Directory)?;
        Ok(())
    };
    for client in ClientId::iter() {
        // Consume scanner-owned vectors; no second whole-history path/sample map.
        for path in std::mem::take(scan.get_mut(client)) {
            if matches!(
                client,
                ClientId::Hermes | ClientId::Zed | ClientId::MiMoCode | ClientId::AntigravityCli
            ) {
                add(&path, ProbeKind::Database, true, &mut roots)?;
                add(&suffix(&path, "-wal"), ProbeKind::Wal, false, &mut roots)?;
                continue;
            }
            add(&path, ProbeKind::File, true, &mut roots)?;
            // Freeze the parser's own dependency enumeration at capture only.
            let fingerprint = match client {
                ClientId::Claude => {
                    match SourceFingerprint::check_claude_code_path_with_home_samples_only(
                        &path,
                        None,
                        Some(context.home_dir()),
                    ) {
                        Some(FingerprintStatus::Changed(fingerprint)) => Some(fingerprint),
                        _ => None,
                    }
                }
                ClientId::Droid => SourceFingerprint::from_droid_path(&path),
                ClientId::Kimi => SourceFingerprint::from_kimi_path(&path),
                ClientId::RooCode | ClientId::KiloCode | ClientId::Cline => {
                    SourceFingerprint::from_roo_path(&path)
                }
                ClientId::Jcode => SourceFingerprint::from_jcode_path(&path),
                ClientId::Grok => SourceFingerprint::from_grok_path(&path),
                ClientId::Kiro => SourceFingerprint::from_kiro_path(&path),
                _ => continue,
            }
            .ok_or_else(|| "source dependency capture failed".to_owned())?;
            for related in fingerprint.related_files {
                add(
                    &related.path.into_path_buf(),
                    ProbeKind::File,
                    false,
                    &mut roots,
                )?;
            }
        }
    }
    for database in crate::sqlite_source_paths(&scan) {
        add(&database, ProbeKind::Database, true, &mut roots)?;
        add(
            &suffix(&database, "-wal"),
            ProbeKind::Wal,
            false,
            &mut roots,
        )?;
    }
    if let Some(database) = &scan.copilot_desktop_db {
        let root = database
            .parent()
            .ok_or("Copilot source parent unavailable")?
            .join("session-state");
        match std::fs::read_dir(&root) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|_| "Copilot dependency traversal failed")?;
                    if !entry
                        .file_type()
                        .map_err(|_| "Copilot dependency metadata failed")?
                        .is_dir()
                    {
                        return Err("Copilot dependency entry is not a directory".into());
                    }
                    add(
                        &entry.path().join("events.jsonl"),
                        ProbeKind::File,
                        false,
                        &mut roots,
                    )?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Copilot dependency traversal failed".into()),
        }
    }
    store
        .db
        .execute_batch("COMMIT")
        .map_err(|_| "source manifest publication failed")?;
    store.admit()?;
    let roots = roots
        .into_iter()
        .map(|(source_root, is_directory)| {
            let watch_root = nearest_directory(&source_root, is_directory)?;
            Ok(LocalSourceWatchRoot {
                source_root,
                watch_root,
                is_directory,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(LocalSourceWatchManifest {
        roots,
        ignored,
        ignored_rules,
        known: Mutex::new(store),
    })
}

/// Parser sibling paths for a hinted primary, mirroring capture's frozen
/// dependency enumeration through the same fingerprint entry points (no new
/// parser path). Derivations that fail yield nothing. The caller only honors
/// siblings that are baseline rows; a coincidental match merely rediscovers.
fn hint_sibling_paths(path: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let mut fingerprints = Vec::new();
    // Mirror capture exactly: Claude resolves parents with home samples.
    if let Some(FingerprintStatus::Changed(fingerprint)) =
        SourceFingerprint::check_claude_code_path_with_home_samples_only(path, None, Some(home_dir))
    {
        fingerprints.push(fingerprint);
    }
    for fingerprint in [
        SourceFingerprint::from_droid_path(path),
        SourceFingerprint::from_kimi_path(path),
        SourceFingerprint::from_roo_path(path),
        SourceFingerprint::from_jcode_path(path),
        SourceFingerprint::from_grok_path(path),
        SourceFingerprint::from_kiro_path(path),
    ]
    .into_iter()
    .flatten()
    {
        fingerprints.push(fingerprint);
    }
    fingerprints
        .into_iter()
        .flat_map(|fingerprint| fingerprint.related_files)
        .map(|related| related.path.into_path_buf())
        .collect()
}

fn database_filename(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("db" | "sqlite" | "sqlite3")
    )
}

fn suffix(path: &Path, value: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(value);
    PathBuf::from(name)
}

fn add_watch_target(
    context: &ResolvedLocalSourceContext,
    ignored: &[PathBuf],
    roots: &mut BTreeMap<PathBuf, bool>,
    path: &Path,
    directory: bool,
) -> Result<(), String> {
    let mut insert = |target: PathBuf| {
        if context.admits_scan_root(&target)
            && !ignored.contains(&target)
            && !roots
                .iter()
                .any(|(root, kind)| *kind && target.starts_with(root))
        {
            roots.insert(target, directory);
        }
    };
    insert(path.to_path_buf());
    if let Some(alias) = resolved_alias(path) {
        insert(alias);
    }
    Ok(())
}

fn resolved_alias(path: &Path) -> Option<PathBuf> {
    // Absent source/WAL/output roots need the same resolved recovery alias.
    let mut ancestor = path;
    loop {
        if let Ok(target) = std::fs::canonicalize(ancestor) {
            return Some(target.join(path.strip_prefix(ancestor).ok()?));
        }
        ancestor = ancestor.parent()?;
    }
}

fn owned_outputs(context: &ResolvedLocalSourceContext) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for root in std::iter::once(context.usage_dir()).chain(context.source_cache_dir()) {
        for name in [
            "source-message-cache.sqlite3",
            "source-message-cache.recovery.sqlite3",
        ] {
            let database = root.join(name);
            paths.insert(database.clone());
            for extension in ["-wal", "-shm", "-journal"] {
                paths.insert(suffix(&database, extension));
            }
        }
        for name in [
            "source-message-cache.sqlite3.init.lock",
            "source-message-cache.recovery.sqlite3.lock",
            "source-message-cache.lock",
            "usage-migration.lock",
            "usage-management.lock",
            "usage-reset.barrier",
        ] {
            paths.insert(root.join(name));
        }
    }
    let pricing = context.pricing_config_dir().join("cache");
    for name in [
        "pricing-litellm.json",
        "pricing-openrouter.json",
        "opencode-migration.json",
    ] {
        paths.insert(pricing.join(name));
    }
    // Preserve native declared and resolved aliases of each exact output.
    for path in paths.clone() {
        if let Some(target) = resolved_alias(&path) {
            paths.insert(target);
        }
    }
    paths.into_iter().collect()
}

fn owned_write_rules(context: &ResolvedLocalSourceContext) -> Vec<LocalSourceOwnedWriteRule> {
    let mut rules = Vec::new();
    let mut add = |parent: PathBuf, pattern: &str| {
        if let Some(alias) = resolved_alias(&parent) {
            if alias != parent {
                rules.push(LocalSourceOwnedWriteRule {
                    parent: alias,
                    pattern: pattern.to_owned(),
                });
            }
        }
        rules.push(LocalSourceOwnedWriteRule {
            parent,
            pattern: pattern.to_owned(),
        });
    };
    for root in std::iter::once(context.usage_dir()).chain(context.source_cache_dir()) {
        add(
            root.to_path_buf(),
            r"^(?:\.usage-restore-|usage_migration_tmp_)[0-9]+-[0-9]+\.sqlite3(?:-wal|-shm|-journal)?$",
        );
    }
    let pricing = context.pricing_config_dir().join("cache");
    add(
        pricing.clone(),
        r"^\.pricing-(?:litellm|openrouter)\.json\.[0-9]+\.[0-9a-f]+\.tmp$",
    );
    add(pricing, r"^\.opencode-migration\.[0-9]+\.[0-9a-f]+\.tmp$");
    rules
}

fn metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::metadata(path) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("source metadata is unreadable".into()),
    }
}

fn nearest_directory(path: &Path, directory: bool) -> Result<PathBuf, String> {
    let mut candidate = if directory {
        path
    } else {
        path.parent().ok_or("source watch parent is unavailable")?
    };
    loop {
        if metadata(candidate)?.is_some_and(|value| value.is_dir()) {
            return Ok(candidate.to_path_buf());
        }
        candidate = candidate
            .parent()
            .ok_or("source watch directory is unavailable")?;
    }
}

fn observe(path: &Path, kind: ProbeKind) -> Result<Option<ObservedFile>, String> {
    let Some(before) = metadata(path)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        // Metadata can succeed for a mode-000 file. Do not mistake it for a
        // readable empty file, including under a privileged test process.
        if before.mode() & 0o444 == 0
            || (kind == ProbeKind::Directory && before.mode() & 0o111 == 0)
        {
            return Err("source dependency is unreadable".into());
        }
        (before.dev(), before.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    if kind == ProbeKind::Directory {
        if !before.is_dir() {
            return Err("source directory changed type".into());
        }
        // Opening the directory verifies accessibility; do not enumerate it or
        // compare child-write mtime (owned output writes are not source dirt).
        std::fs::read_dir(path).map_err(|_| "source directory is unreadable")?;
        return Ok(Some(ObservedFile {
            size: 0,
            modified: None,
            identity,
            samples: Vec::new(),
        }));
    }
    if !before.is_file() {
        return Err("source dependency changed type".into());
    }
    let size = before.len();
    let offsets = sample_offsets(size);
    let mut samples = Vec::with_capacity(offsets.len());
    let mut buffer = [0_u8; 4096];
    if matches!(kind, ProbeKind::Database | ProbeKind::Wal) {
        let file = VfsFile::open(path, kind == ProbeKind::Wal)?;
        for (offset, len) in offsets {
            file.read(offset, &mut buffer[..len])?;
            samples.push(hash_bytes(&buffer[..len]));
        }
    } else {
        let mut file = std::fs::File::open(path).map_err(|_| "source dependency is unreadable")?;
        for (offset, len) in offsets {
            file.seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut buffer[..len]))
                .map_err(|_| "source dependency sampling failed")?;
            samples.push(hash_bytes(&buffer[..len]));
        }
    }
    let after = metadata(path)?.ok_or("source disappeared during verification")?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("source changed during verification".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if identity != (after.dev(), after.ino()) {
            return Err("source identity changed during verification".into());
        }
    }
    // Same SQLite no-frame equivalence as the parser fingerprint. Readability
    // was checked above, so an unreadable zero WAL is never normalized away.
    if kind == ProbeKind::Wal && size == 0 {
        return Ok(None);
    }
    Ok(Some(ObservedFile {
        size,
        modified: Some(
            before
                .modified()
                .map_err(|_| "source timestamp unavailable")?,
        ),
        identity,
        samples,
    }))
}

/// SQLite VFS owns every descriptor for lock-bearing inodes. An ordinary
/// File::open/close can drop unrelated POSIX locks held by another connection
/// in this process. xClose instead participates in SQLite's inode bookkeeping.
struct VfsFile {
    storage: Vec<usize>,
    // SQLite allocates its extended filename/URI representation; VFS methods
    // may retain this pointer or use sqlite3_filename_database() on a sidecar.
    name: rusqlite::ffi::sqlite3_filename,
}

impl VfsFile {
    fn open(path: &Path, wal: bool) -> Result<Self, String> {
        #[cfg(unix)]
        let bytes = {
            use std::os::unix::ffi::OsStrExt;
            path.as_os_str().as_bytes()
        };
        #[cfg(not(unix))]
        let bytes = path
            .to_str()
            .ok_or("SQLite source path is not representable by its VFS")?
            .as_bytes();
        let database = if wal {
            Some(
                std::ffi::CString::new(
                    bytes
                        .strip_suffix(b"-wal")
                        .ok_or("SQLite WAL path invalid")?,
                )
                .map_err(|_| "SQLite source path contains NUL")?,
            )
        } else {
            None
        };
        // SQLite's raw VFS rejects symlinks; its normal connection opener
        // resolves them before xOpen. Keep the declared DB/WAL association,
        // but open the resolved file through the VFS for lock-safe sampling.
        let resolved =
            std::fs::canonicalize(path).map_err(|_| "SQLite source path resolution failed")?;
        #[cfg(unix)]
        let resolved_bytes = {
            use std::os::unix::ffi::OsStrExt;
            resolved.as_os_str().as_bytes()
        };
        #[cfg(not(unix))]
        let resolved_bytes = resolved
            .to_str()
            .ok_or("SQLite source path is not representable by its VFS")?
            .as_bytes();
        let name = std::ffi::CString::new(resolved_bytes)
            .map_err(|_| "SQLite source path contains NUL")?;
        // SAFETY: SQLite initialization and default VFS lookup are thread-safe.
        let vfs = unsafe {
            if rusqlite::ffi::sqlite3_initialize() != rusqlite::ffi::SQLITE_OK {
                return Err("SQLite VFS initialization failed".into());
            }
            rusqlite::ffi::sqlite3_vfs_find(std::ptr::null())
        };
        // SAFETY: the default VFS remains registered for SQLite's lifetime.
        let vfs_ref = unsafe { vfs.as_ref() }.ok_or("SQLite source VFS unavailable")?;
        let size =
            usize::try_from(vfs_ref.szOsFile).map_err(|_| "SQLite source VFS size invalid")?;
        if size < std::mem::size_of::<rusqlite::ffi::sqlite3_file>() {
            return Err("SQLite source VFS size invalid".into());
        }
        let open = vfs_ref.xOpen.ok_or("SQLite source VFS open unavailable")?;
        // SAFETY: all names are terminated, N=0 admits null parameters; SQLite
        // copies them into its required extended VFS filename representation.
        let filename = unsafe {
            rusqlite::ffi::sqlite3_create_filename(
                database.as_ref().unwrap_or(&name).as_ptr(),
                c"".as_ptr(),
                if wal { name.as_ptr() } else { c"".as_ptr() },
                0,
                std::ptr::null_mut(),
            )
        };
        if filename.is_null() {
            return Err("SQLite source filename allocation failed".into());
        }
        let file = Self {
            storage: vec![0; size.div_ceil(std::mem::size_of::<usize>())],
            name: filename,
        };
        // SAFETY: both names belong to the live SQLite-owned allocation.
        let open_name = if wal {
            unsafe { rusqlite::ffi::sqlite3_filename_wal(file.name) }
        } else {
            file.name
        };
        let flags = rusqlite::ffi::SQLITE_OPEN_READONLY
            | if wal {
                rusqlite::ffi::SQLITE_OPEN_WAL
            } else {
                rusqlite::ffi::SQLITE_OPEN_MAIN_DB
            };
        let mut output_flags = 0;
        // SAFETY: storage is aligned and szOsFile-sized, zName stays alive,
        // output_flags is writable; xOpen installs this VFS's matching methods.
        let result = unsafe { open(vfs, open_name, file.pointer(), flags, &mut output_flags) };
        if result != rusqlite::ffi::SQLITE_OK {
            return Err("SQLite source VFS open failed".into());
        }
        Ok(file)
    }

    fn pointer(&self) -> *mut rusqlite::ffi::sqlite3_file {
        self.storage.as_ptr().cast_mut().cast()
    }

    #[cfg(windows)]
    fn windows_identity(&self) -> Result<WindowsFileIdentity, String> {
        // SAFETY: the live owned VFS file installed these methods at xOpen.
        let methods = unsafe { (*self.pointer()).pMethods.as_ref() }
            .ok_or("SQLite source VFS methods unavailable")?;
        let control = methods
            .xFileControl
            .ok_or("SQLite source VFS control unavailable")?;
        let mut handle = std::ptr::null_mut();
        // SAFETY: the VFS writes its borrowed HANDLE; it stays owned by this file.
        let status = unsafe {
            control(
                self.pointer(),
                rusqlite::ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&mut handle as *mut *mut std::ffi::c_void).cast(),
            )
        };
        if status != rusqlite::ffi::SQLITE_OK {
            return Err("SQLite source Windows handle unavailable".into());
        }
        windows_file_identity(handle)
    }

    fn read(&self, offset: u64, buffer: &mut [u8]) -> Result<(), String> {
        // SAFETY: the live owned VFS file installed these methods at xOpen.
        let methods = unsafe { (*self.pointer()).pMethods.as_ref() }
            .ok_or("SQLite source VFS methods unavailable")?;
        let read = methods.xRead.ok_or("SQLite source VFS read unavailable")?;
        let offset =
            i64::try_from(offset).map_err(|_| "SQLite source sample offset out of range")?;
        let len =
            i32::try_from(buffer.len()).map_err(|_| "SQLite source sample size out of range")?;
        // SAFETY: output buffer has len bytes, file belongs to read's VFS.
        let status = unsafe { read(self.pointer(), buffer.as_mut_ptr().cast(), len, offset) };
        if status == rusqlite::ffi::SQLITE_OK {
            Ok(())
        } else {
            Err("SQLite source VFS sample failed".into())
        }
    }
}

impl Drop for VfsFile {
    fn drop(&mut self) {
        // SAFETY: this is the sole xClose for the matching xOpen. A failed
        // xOpen may leave methods null; SQLite's contract allows that case.
        unsafe {
            if let Some(methods) = (*self.pointer()).pMethods.as_ref() {
                if let Some(close) = methods.xClose {
                    close(self.pointer());
                }
            }
            rusqlite::ffi::sqlite3_free_filename(self.name);
        }
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default, PartialEq, Eq)]
struct WindowsFileIdentity {
    volume: u64,
    file_id: [u8; 16],
}

#[cfg(windows)]
fn windows_file_identity(handle: *mut std::ffi::c_void) -> Result<WindowsFileIdentity, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    // FILE_ID_INFO includes the full ReFS identifier, not truncated legacy IDs.
    const FILE_ID_INFO: i32 = 18;
    let mut identity = WindowsFileIdentity::default();
    // SAFETY: SQLite owns the live HANDLE; the ABI-compatible output is exactly
    // FILE_ID_INFO-sized. This call borrows the handle and never closes it.
    let status = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FILE_ID_INFO,
            (&mut identity as *mut WindowsFileIdentity).cast(),
            std::mem::size_of::<WindowsFileIdentity>() as u32,
        )
    };
    if status == 0 {
        return Err("source manifest Windows file identity unavailable".into());
    }
    Ok(identity)
}

#[cfg(test)]
#[path = "source_watch_tests.rs"]
mod tests;
