use crate::message_cache::{
    CacheIdentity, CachedSourceMetadata, SourceFingerprint, SourceMessageCache,
};
use crate::{sessions, ClientId, UnifiedMessage};
use bincode::Options;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// Durable Report winner index marker. Version 1 stays LocalClients-only; a
/// Report generation's own selected Claude/Codex references live under this
/// version and never borrow LocalClients ownership.
const REPORT_INDEX_VERSION: i64 = 2;

/// NULL inherits immutable canonical metadata. Explicit absent workspaces remain
/// encoded Option::None blobs; legacy SQL NULL was never decodable metadata.
pub(crate) fn compact_decision_metadata(db: &Connection) -> Result<(), String> {
    let present: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sq_decisions')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !present {
        return Ok(());
    }
    let referenced: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_decisions WHERE cache_source_id IS NOT NULL)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !referenced {
        return Ok(());
    }
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if version == 1 {
        let invalid: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_decisions WHERE cache_source_id IS NOT NULL AND (workspace IS NULL OR workspace_label IS NULL OR model IS NULL))",
            [], |row| row.get(0),
        ).map_err(|error| error.to_string())?;
        if invalid {
            return Err("legacy usage decision metadata is NULL; previous values retained".into());
        }
    }
    let mapped: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='mc_generation_storage')",
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    let storage = if mapped {
        "mc_generation_storage"
    } else {
        "(SELECT source_id,generation,generation AS row_generation,9223372036854775807 AS row_count FROM mc_versions)"
    };
    let canonical = format!(
        "{storage} st JOIN mc_rows r ON r.source_id=st.source_id AND r.generation=st.row_generation"
    );
    let missing: bool = db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM sq_decisions d WHERE d.cache_source_id IS NOT NULL AND NOT EXISTS(SELECT 1 FROM {canonical} WHERE st.source_id=d.cache_source_id AND st.generation=d.cache_generation AND r.ordinal=d.cache_ordinal AND r.ordinal<st.row_count))"),
        [], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if missing {
        return Err(
            "usage decision canonical metadata unavailable; previous values retained".into(),
        );
    }
    db.execute(
        &format!("UPDATE sq_decisions AS d SET
            workspace=NULLIF(d.workspace,r.workspace),
            workspace_label=NULLIF(d.workspace_label,r.workspace_label),
            model=NULLIF(d.model,r.model_id)
            FROM {canonical}
            WHERE st.source_id=d.cache_source_id AND st.generation=d.cache_generation
              AND r.ordinal=d.cache_ordinal AND r.ordinal<st.row_count
              AND (d.workspace=r.workspace OR d.workspace_label=r.workspace_label OR d.model=r.model_id)"),
        [],
    ).map_err(|error| error.to_string())?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SelectionMode {
    Report,
    LocalClients,
    UnifiedCompatibility,
}

#[derive(Clone, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub(crate) root: std::path::PathBuf,
    pub(crate) scope: Vec<u8>,
    pub(crate) generation: i64,
    store_epoch: [u8; 16],
    pub(crate) clients: Vec<String>,
    pub(crate) mode: SelectionMode,
    pub(crate) selection_ignores_filter: bool,
    pub(crate) modified_after: Option<u64>,
    preparation: ReportPreparationSlot,
}

// Equality describes producer identity, never disposable connection-local work.
#[derive(Clone, Default)]
struct ReportPreparationSlot(std::sync::Arc<std::sync::Mutex<Option<PreparedHistory>>>);

impl PartialEq for ReportPreparationSlot {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ReportPreparationSlot {}

static RETAINED_HISTORY_PREPARATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
const MAX_RETAINED_HISTORY_PREPARATIONS: usize = 2;

struct PreparationIdentity {
    root: PathBuf,
    scope: Vec<u8>,
    generation: i64,
    store_epoch: [u8; 16],
    clients: Vec<String>,
    mode: SelectionMode,
    selection_ignores_filter: bool,
    modified_after: Option<u64>,
}

impl PreparationIdentity {
    fn matches(&self, snapshot: &UsageSnapshot) -> bool {
        self.root == snapshot.root
            && self.scope == snapshot.scope
            && self.generation == snapshot.generation
            && self.store_epoch == snapshot.store_epoch
            && self.clients == snapshot.clients
            && self.mode == snapshot.mode
            && self.selection_ignores_filter == snapshot.selection_ignores_filter
            && self.modified_after == snapshot.modified_after
    }
}

struct PreparedHistory {
    query: QueryState,
    identity: PreparationIdentity,
    data_version: i64,
    retained: bool,
    report_ready: bool,
}

impl Drop for PreparedHistory {
    fn drop(&mut self) {
        if self.retained {
            RETAINED_HISTORY_PREPARATIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Exclusively borrow a preparation without holding a mutex during a fold.
/// Concurrent/reentrant consumers use independent scratch, never shared TEMP state.
pub(crate) struct HistoricalQuery<'a> {
    prepared: Option<PreparedHistory>,
    slot: &'a ReportPreparationSlot,
}

impl std::ops::Deref for HistoricalQuery<'_> {
    type Target = QueryState;
    fn deref(&self) -> &QueryState {
        &self
            .prepared
            .as_ref()
            .expect("historical query is live")
            .query
    }
}

impl HistoricalQuery<'_> {
    pub(crate) fn admit_reader(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        if self.read_pinned.get() || !self.canonical_payloads.get() {
            return Ok(());
        }
        self.pin(snapshot)?;
        let admitted = (|| {
            let prepared = self.prepared.as_ref().expect("historical query is live");
            if !crate::usage_store::admit_retained_database(self.connection(), &snapshot.root)?
                || self.data_version()? != prepared.data_version
            {
                return Err("historical preparation changed; synchronize and reissue".into());
            }
            Ok(())
        })();
        if admitted.is_err() {
            let released = self.unpin();
            return admitted.and(released);
        }
        Ok(())
    }

    pub(crate) fn release_reader(&self) -> Result<(), String> {
        if self.read_pinned.get() {
            self.unpin()?;
        }
        Ok(())
    }
}

impl Drop for HistoricalQuery<'_> {
    fn drop(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        let released = !prepared.query.read_pinned.get() || prepared.query.unpin().is_ok();
        if released && prepared.retained && !std::thread::panicking() {
            let mut slot = self
                .slot
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                *slot = Some(prepared);
            }
        }
    }
}
impl UsageSnapshot {
    /// Reuse only connection-local work. The same global retention cap covers
    /// recent and historical readers; reentrant calls still open private state.
    pub(crate) fn recent_query(&self) -> Result<HistoricalQuery<'_>, String> {
        use std::sync::atomic::Ordering;
        // Platforms without retained-inode admission keep the original fresh,
        // disposable query path rather than retaining an unverifiable handle.
        let local = cfg!(unix) && self.mode == SelectionMode::LocalClients;
        let cached = if local {
            self.preparation
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        } else {
            None
        };
        let prepared =
            match cached.filter(|cached| !cached.report_ready && cached.identity.matches(self)) {
                Some(cached) => cached,
                None => {
                    let query = QueryState::open(&self.root)?;
                    let retained = local
                        && RETAINED_HISTORY_PREPARATIONS
                            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                                (count < MAX_RETAINED_HISTORY_PREPARATIONS).then_some(count + 1)
                            })
                            .is_ok();
                    PreparedHistory {
                        query,
                        identity: PreparationIdentity {
                            root: self.root.clone(),
                            scope: self.scope.clone(),
                            generation: self.generation,
                            store_epoch: self.store_epoch,
                            clients: self.clients.clone(),
                            mode: self.mode,
                            selection_ignores_filter: self.selection_ignores_filter,
                            modified_after: self.modified_after,
                        },
                        data_version: 0,
                        retained,
                        report_ready: false,
                    }
                }
            };
        prepared.query.pin(self)?;
        // Unix fresh and retained readers both keep full inode admission.
        // Non-Unix readers are always fresh, admitted by open_database + pin.
        #[cfg(unix)]
        if !crate::usage_store::admit_retained_database(prepared.query.connection(), &self.root)? {
            return Err("usage database changed; synchronize and reissue".into());
        }
        Ok(HistoricalQuery {
            prepared: Some(prepared),
            slot: &self.preparation,
        })
    }
    pub(crate) fn historical_query(&self) -> Result<HistoricalQuery<'_>, String> {
        use std::sync::atomic::Ordering;
        let cached = self
            .preparation
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let mut prepared =
            cached.filter(|cached| cached.report_ready && cached.identity.matches(self));
        if let Some(cached) = &prepared {
            cached.query.pin(self)?;
            let admission = (|| {
                Ok::<_, String>((
                    crate::usage_store::admit_retained_database(
                        cached.query.connection(),
                        &self.root,
                    )?,
                    cached.query.data_version()?,
                ))
            })();
            let (identity_available, version) = admission?;
            if !identity_available || version != cached.data_version {
                cached.query.unpin()?;
                prepared = None;
            }
        }
        if prepared.is_none() {
            let query = QueryState::open(&self.root)?;
            crate::usage_store::ReportCheckpoint::initialize(query.connection())?;
            query.pin(self)?;
            // Capture while pinned: external writes invalidate the next
            // checkout, while this fold keeps its admitted canonical snapshot.
            let result = (|| {
                let version = query.data_version()?;
                query.prepare_trusted_query(self, UsageQuery::compact())?;
                query.prepare_report_fold(self)?;
                Ok::<_, String>(version)
            })();
            let data_version = result?;
            let identity = PreparationIdentity {
                root: self.root.clone(),
                scope: self.scope.clone(),
                generation: self.generation,
                store_epoch: self.store_epoch,
                clients: self.clients.clone(),
                mode: self.mode,
                selection_ignores_filter: self.selection_ignores_filter,
                modified_after: self.modified_after,
            };
            let retained = RETAINED_HISTORY_PREPARATIONS
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    (count < MAX_RETAINED_HISTORY_PREPARATIONS).then_some(count + 1)
                })
                .is_ok();
            prepared = Some(PreparedHistory {
                query,
                identity,
                data_version,
                retained,
                report_ready: true,
            });
        }
        // Immutable rows/membership survive. OpenCode's visitor resets its own
        // arbitration; only checkpoint staging needs a per-report reset here.
        // Reset before the return-to-slot guard so errors discard preparation.
        prepared
            .as_ref()
            .expect("historical preparation is ready")
            .query
            .connection()
            .execute_batch("DROP TABLE IF EXISTS temp.report_checkpoint_staged;")
            .map_err(|error| error.to_string())?;
        let result = HistoricalQuery {
            prepared,
            slot: &self.preparation,
        };
        Ok(result)
    }
    pub fn generation(&self) -> u64 {
        self.generation as u64
    }
    /// Retained native metadata plus conservative historical pager allowance.
    pub fn metadata_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            // Conservative per-handle pager allowance, including an in-flight
            // preparation removed from the slot. At most two are retained globally.
            + 320 * 1024
            + self.root.capacity()
            + self.scope.capacity()
            + self.clients.capacity() * std::mem::size_of::<String>()
            + self.clients.iter().map(String::capacity).sum::<usize>()
    }
    pub fn scope_identity(&self) -> String {
        use std::fmt::Write;
        let mut result = String::with_capacity(self.scope.len() * 2);
        for byte in &self.scope {
            let _ = write!(&mut result, "{byte:02x}");
        }
        result
    }
}

impl std::fmt::Debug for UsageSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalUsageSnapshot")
            .field("generation", &self.generation)
            .field("scope_identity", &self.scope_identity())
            .field("selection", &self.mode)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UsageQuery {
    /// Inclusive Unix-millisecond lower bound, after identity selection.
    pub from_ms: Option<i64>,
    /// Exclusive Unix-millisecond upper bound, after identity selection.
    pub until_ms: Option<i64>,
    /// Timestamp order with original lane/traversal order for ties.
    pub chronological: bool,
    /// Public message visitors preserve aliases by default. A requested alias
    /// cohort may be one oversized output row; selection state still spills.
    pub hydrate_opencode_aliases: bool,
}

impl Default for UsageQuery {
    fn default() -> Self {
        Self {
            from_ms: None,
            until_ms: None,
            chronological: false,
            hydrate_opencode_aliases: true,
        }
    }
}
impl UsageQuery {
    pub(crate) const fn compact() -> Self {
        Self {
            from_ms: None,
            until_ms: None,
            chronological: false,
            hydrate_opencode_aliases: false,
        }
    }
    pub(crate) fn contains(&self, timestamp: i64) -> bool {
        self.from_ms.is_none_or(|from| timestamp >= from)
            && self.until_ms.is_none_or(|until| timestamp < until)
    }
}

/// Published ownership decisions persist as references, never payloads.
/// Public callbacks detach bytes; trusted reports keep an admitted read pin.
pub(crate) struct QueryState {
    db: Connection,
    store_epoch: [u8; 16],
    pending_scope: std::cell::Cell<Option<i64>>,
    pending_mode: std::cell::Cell<SelectionMode>,
    read_pinned: std::cell::Cell<bool>,
    canonical_payloads: std::cell::Cell<bool>,
    indexed: std::cell::Cell<bool>,
    report_reused: std::cell::Cell<bool>,
    complete_sources: std::cell::Cell<Option<(i64, i64, u64)>>,
    seeded_cache_descriptors: std::cell::Cell<bool>,
    #[cfg(test)]
    completeness_checks: std::cell::Cell<u64>,
}

pub(crate) struct IncrementalSource {
    pub(crate) client: ClientId,
    pub(crate) index: usize,
    pub(crate) headless: bool,
    pub(crate) tag: bool,
}

impl QueryState {
    fn data_version(&self) -> Result<i64, String> {
        self.db
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn open(root: &Path) -> Result<Self, String> {
        let db = crate::usage_store::open_database(root)?;
        let existing:i64=db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('sq_meta','sq_scopes','sq_scope_generations','sq_scope_sources')",[],|row|row.get(0)).map_err(|error|error.to_string())?;
        if existing != 0 && existing != 4 {
            return Err("usage scope schema is incomplete; previous generations retained".into());
        }
        if existing == 0 {
            let transaction =
                rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| error.to_string())?;
            transaction.execute_batch("CREATE TABLE IF NOT EXISTS sq_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
            INSERT OR IGNORE INTO sq_meta VALUES('schema_version',5);
            INSERT OR IGNORE INTO sq_meta VALUES('store_epoch',randomblob(16));
            CREATE TABLE IF NOT EXISTS sq_scopes(id INTEGER PRIMARY KEY,scope_key BLOB NOT NULL UNIQUE,current_generation INTEGER);
            CREATE TABLE IF NOT EXISTS sq_scope_generations(id INTEGER PRIMARY KEY,scope_id INTEGER NOT NULL,complete INTEGER NOT NULL DEFAULT 0,selection INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS sq_scope_sources(scope_generation INTEGER NOT NULL,kind TEXT NOT NULL,lane TEXT NOT NULL,source_order INTEGER NOT NULL,path BLOB NOT NULL,cache_source_id INTEGER,cache_generation INTEGER,generation INTEGER,tag INTEGER NOT NULL,headless INTEGER NOT NULL,workspace BLOB,workspace_label BLOB,phase INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scope_generation,kind,lane,source_order)) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS sq_scope_cache_versions ON sq_scope_sources(cache_source_id,cache_generation);
            CREATE INDEX IF NOT EXISTS sq_scope_opencode_versions ON sq_scope_sources(kind,path,generation);").map_err(|error|error.to_string())?;
            transaction.commit().map_err(|error| error.to_string())?;
        }
        let version: i64 = db
            .query_row(
                "SELECT value FROM sq_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !matches!(version, 3..=5) {
            return Err("usage scope schema is unsupported; previous generations retained".into());
        }
        db.prepare("SELECT g.id,g.scope_id,g.complete,g.selection,s.id,s.scope_key,s.current_generation,x.scope_generation,x.kind,x.lane,x.source_order,x.path,x.cache_source_id,x.cache_generation,x.generation,x.tag,x.headless,x.workspace,x.workspace_label,x.phase FROM sq_scope_generations g LEFT JOIN sq_scopes s ON s.id=g.scope_id LEFT JOIN sq_scope_sources x ON x.scope_generation=g.id LIMIT 0").map_err(|error|error.to_string())?;
        let store_epoch = if let Some(epoch) = Self::read_store_epoch(&db)? {
            epoch
        } else {
            // Adopt existing scope metadata once; warm queries stay read-only.
            let transaction =
                rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO sq_meta VALUES('store_epoch',randomblob(16))",
                    [],
                )
                .map_err(|error| error.to_string())?;
            let epoch = Self::read_store_epoch(&transaction)?
                .ok_or("usage store epoch is missing; previous generations retained")?;
            transaction.commit().map_err(|error| error.to_string())?;
            epoch
        };
        let indexed_tables: i64 = db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('sq_indexes','sq_decisions')", [], |r| r.get(0)).map_err(|e| e.to_string())?;
        if indexed_tables == 0 {
            let transaction =
                rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                    .map_err(|e| e.to_string())?;
            transaction.execute_batch("CREATE TABLE IF NOT EXISTS sq_indexes(scope_generation INTEGER PRIMARY KEY,version INTEGER NOT NULL,decision_generation INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS sq_decisions(scope_generation INTEGER NOT NULL,rank INTEGER NOT NULL,lane TEXT NOT NULL,timestamp INTEGER NOT NULL,cache_source_id INTEGER,cache_generation INTEGER,cache_ordinal INTEGER,cache_format INTEGER,oc_source INTEGER,oc_ordinal INTEGER,retained INTEGER NOT NULL DEFAULT 0,global_storage INTEGER NOT NULL DEFAULT 0,workspace BLOB,workspace_label BLOB,model TEXT,headless INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scope_generation,rank)) WITHOUT ROWID;").map_err(|e| e.to_string())?;
            transaction.commit().map_err(|e| e.to_string())?;
        } else if indexed_tables != 2 {
            return Err("usage decision index is incomplete; previous generations retained".into());
        }
        let integer_references: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('sq_scope_sources') WHERE name='generation' AND upper(type)='INTEGER')
             AND EXISTS(SELECT 1 FROM pragma_table_info('sq_decisions') WHERE name='oc_source' AND upper(type)='INTEGER')",
            [], |row| row.get(0),
        ).map_err(|error| error.to_string())?;
        if !integer_references {
            return Err(
                "usage scope reference layout is unsupported; previous generations retained".into(),
            );
        }
        let needs_format: bool = db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('sq_decisions') WHERE name='cache_format')",
            [], |row| row.get(0),
        ).map_err(|error| error.to_string())?;
        if needs_format {
            let transaction =
                rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| error.to_string())?;
            let still_missing: bool = transaction.query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('sq_decisions') WHERE name='cache_format')",
                [], |row| row.get(0),
            ).map_err(|error| error.to_string())?;
            if still_missing {
                transaction
                    .execute_batch(
                        "ALTER TABLE sq_decisions ADD COLUMN cache_format INTEGER;
                     UPDATE sq_decisions SET cache_format=(
                         SELECT v.format_version FROM mc_versions v
                         WHERE v.source_id=sq_decisions.cache_source_id
                           AND v.generation=sq_decisions.cache_generation
                     ) WHERE cache_source_id IS NOT NULL;",
                    )
                    .map_err(|error| error.to_string())?;
            }
            transaction.commit().map_err(|error| error.to_string())?;
        }
        if version != 5 {
            let transaction =
                rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| error.to_string())?;
            let current: i64 = transaction
                .query_row(
                    "SELECT value FROM sq_meta WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            match current {
                3 | 4 => {
                    transaction
                        .execute_batch("DROP INDEX IF EXISTS sq_decision_time;")
                        .map_err(|error| error.to_string())?;
                    let missing_owner: bool = transaction.query_row(
                        "SELECT NOT EXISTS(SELECT 1 FROM pragma_table_info('sq_indexes') WHERE name='decision_generation')",
                        [], |row| row.get(0),
                    ).map_err(|error| error.to_string())?;
                    if missing_owner {
                        transaction
                            .execute_batch(
                                "ALTER TABLE sq_indexes ADD COLUMN decision_generation INTEGER;
                             UPDATE sq_indexes SET decision_generation=scope_generation;",
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    transaction.execute_batch(
                        "DELETE FROM sq_decisions WHERE oc_source IS NULL
                           AND scope_generation IN (SELECT decision_generation FROM sq_indexes WHERE version=1);
                         UPDATE sq_indexes SET version=3 WHERE version=1;
                         UPDATE sq_meta SET value=5 WHERE key='schema_version';",
                    ).map_err(|error| error.to_string())?;
                }
                5 => {}
                _ => {
                    return Err(
                        "usage scope schema is unsupported; previous generations retained".into(),
                    )
                }
            }
            transaction.commit().map_err(|error| error.to_string())?;
        }
        db.prepare("SELECT decision_generation FROM sq_indexes LIMIT 0")
            .map_err(|error| error.to_string())?;
        // ponytail: scan rank-clustered reference metadata for recent bounds;
        // bulk-build a time lookup only if measured read CPU requires it.
        // 64 KiB TEMP budget, aligned with the durable-store TEMP budget; RAM over sort latency.
        db.execute_batch("PRAGMA temp.cache_size=-64;
            CREATE TEMP TABLE sq_rows(id INTEGER PRIMARY KEY,lane TEXT NOT NULL,source_order INTEGER NOT NULL,phase INTEGER NOT NULL,retained INTEGER NOT NULL,global_storage INTEGER NOT NULL,session TEXT NOT NULL,workspace BLOB NOT NULL,dedup TEXT,model TEXT NOT NULL,workspace_label BLOB NOT NULL,timestamp INTEGER NOT NULL,payload BLOB,client TEXT NOT NULL DEFAULT '',provider TEXT NOT NULL DEFAULT '',headless INTEGER NOT NULL DEFAULT 0,payload_digest BLOB,eligible INTEGER NOT NULL DEFAULT 1,cache_source_id INTEGER,cache_generation INTEGER,cache_ordinal INTEGER,cache_format INTEGER);
            CREATE TEMP VIEW sq_payload_rows AS SELECT * FROM sq_rows;
            CREATE INDEX sq_lane_order ON sq_rows(lane,phase,source_order,id);
            CREATE INDEX sq_session ON sq_rows(lane,session);
            CREATE TEMP TABLE sq_selected(rank INTEGER PRIMARY KEY,id INTEGER NOT NULL UNIQUE);
            CREATE TEMP TABLE sq_output(rank INTEGER PRIMARY KEY,timestamp INTEGER NOT NULL,payload BLOB NOT NULL);
            CREATE INDEX sq_output_time ON sq_output(timestamp,rank);
            CREATE TEMP TABLE sq_recent_rows(kind TEXT NOT NULL,lane TEXT NOT NULL,source_order INTEGER NOT NULL,ordinal INTEGER NOT NULL,PRIMARY KEY(kind,lane,source_order,ordinal)) WITHOUT ROWID;
            CREATE TEMP TABLE sq_recent_keys(lane TEXT NOT NULL,dedup TEXT NOT NULL,PRIMARY KEY(lane,dedup)) WITHOUT ROWID;
            CREATE TEMP TABLE sq_sources(lane TEXT NOT NULL,source_order INTEGER NOT NULL,miss INTEGER NOT NULL,seeded_seen INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(lane,source_order)) WITHOUT ROWID;")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            db,
            store_epoch,
            pending_scope: std::cell::Cell::new(None),
            pending_mode: std::cell::Cell::new(SelectionMode::Report),
            read_pinned: std::cell::Cell::new(false),
            canonical_payloads: std::cell::Cell::new(false),
            indexed: std::cell::Cell::new(false),
            report_reused: std::cell::Cell::new(false),
            complete_sources: std::cell::Cell::new(None),
            seeded_cache_descriptors: std::cell::Cell::new(false),
            #[cfg(test)]
            completeness_checks: std::cell::Cell::new(0),
        })
    }
    fn read_store_epoch(db: &Connection) -> Result<Option<[u8; 16]>, String> {
        db.query_row(
            "SELECT value FROM sq_meta WHERE key='store_epoch'",
            [],
            |row| {
                let value = row.get_ref(0)?;
                match value {
                    rusqlite::types::ValueRef::Blob(bytes) if bytes.len() == 16 => {
                        let mut epoch = [0; 16];
                        epoch.copy_from_slice(bytes);
                        Ok(epoch)
                    }
                    _ => Err(rusqlite::Error::InvalidColumnType(
                        0,
                        "store_epoch".into(),
                        value.data_type(),
                    )),
                }
            },
        )
        .optional()
        .map_err(|_| "usage store epoch is invalid; previous generations retained".into())
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.db
    }

    pub(crate) fn pending_generation(&self) -> Result<i64, String> {
        self.pending_scope
            .get()
            .ok_or_else(|| "source publication has no staged scope".to_string())
    }

    fn admit_staging(&self, db: &Connection) -> Result<i64, String> {
        if Self::read_store_epoch(db)? != Some(self.store_epoch) {
            return Err("usage store was reset; synchronize and reissue".into());
        }
        let generation = self
            .pending_scope
            .get()
            .ok_or("source publication has no staged scope")?;
        let active: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_scope_generations WHERE id=?1 AND complete=0)",
                [generation],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !active {
            return Err("staged scope was evicted; synchronize and reissue".into());
        }
        Ok(generation)
    }

    pub(crate) fn begin_scope(
        &self,
        root: &Path,
        context: &crate::ResolvedLocalSourceContext,
        clients: &[String],
        modified_after: Option<u64>,
        mode: SelectionMode,
    ) -> Result<UsageSnapshot, String> {
        use sha2::{Digest, Sha256};
        let scope = Sha256::digest(
            bincode::serialize(&(context.identity_string(), clients, modified_after, mode))
                .map_err(|error| error.to_string())?,
        )
        .to_vec();
        if self.pending_scope.get().is_some() {
            return Err("usage synchronization already has a staged scope".into());
        }
        let transaction = self
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO sq_scopes(scope_key) VALUES(?1)",
                [&scope],
            )
            .map_err(|error| error.to_string())?;
        transaction.execute("INSERT INTO sq_scope_generations(scope_id,selection) SELECT id,?2 FROM sq_scopes WHERE scope_key=?1",params![scope,mode as i32]).map_err(|error|error.to_string())?;
        let generation = transaction.last_insert_rowid();
        transaction.execute_batch("DELETE FROM sq_scope_sources WHERE scope_generation IN (SELECT id FROM sq_scope_generations WHERE complete=0 ORDER BY id DESC LIMIT -1 OFFSET 32);
            DELETE FROM sq_scope_generations WHERE complete=0 AND id NOT IN (SELECT id FROM sq_scope_generations WHERE complete=0 ORDER BY id DESC LIMIT 32);
            DELETE FROM sq_scope_sources WHERE NOT EXISTS(SELECT 1 FROM sq_scope_generations g WHERE g.id=sq_scope_sources.scope_generation);
            DELETE FROM sq_scopes WHERE current_generation IS NULL AND NOT EXISTS(SELECT 1 FROM sq_scope_generations g WHERE g.scope_id=sq_scopes.id);").map_err(|error|error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        self.pending_scope.set(Some(generation));
        self.pending_mode.set(mode);
        Ok(UsageSnapshot {
            root: root.to_path_buf(),
            scope,
            generation,
            store_epoch: self.store_epoch,
            clients: clients.to_vec(),
            mode,
            selection_ignores_filter: context.selection_ignores_report_filter(),
            modified_after,
            preparation: ReportPreparationSlot::default(),
        })
    }
    /// Resolve supported primary sources without changing the published scope.
    /// Siblings, archives and unsupported producers require full ingestion.
    pub(crate) fn incremental_sources(
        &self,
        snapshot: &UsageSnapshot,
        paths: &[PathBuf],
    ) -> Result<Option<Vec<IncrementalSource>>, String> {
        if self.pin(snapshot).is_err() {
            return Ok(None);
        }
        let result = (|| {
            let mut statement = self
                .db
                .prepare(
                    "SELECT kind,lane,source_order,tag,headless,phase,workspace,workspace_label
                 FROM sq_scope_sources WHERE scope_generation=?1 AND path=?2",
                )
                .map_err(|error| error.to_string())?;
            let mut sources = Vec::with_capacity(paths.len());
            for path in paths {
                let encoded = bincode::options()
                    .serialize(&crate::message_cache::CachedPath::from_path(path))
                    .map_err(|error| error.to_string())?;
                let mut rows = statement
                    .query(params![snapshot.generation, encoded])
                    .map_err(|error| error.to_string())?;
                let Some(row) = rows.next().map_err(|error| error.to_string())? else {
                    return Ok(None);
                };
                let kind = row.get_ref(0).map_err(|error| error.to_string())?;
                let kind = kind.as_str().map_err(|error| error.to_string())?;
                let lane = row.get_ref(1).map_err(|error| error.to_string())?;
                let lane = lane.as_str().map_err(|error| error.to_string())?;
                let index: usize = row.get(2).map_err(|error| error.to_string())?;
                let tag: bool = row.get(3).map_err(|error| error.to_string())?;
                let headless: bool = row.get(4).map_err(|error| error.to_string())?;
                let phase: i32 = row.get(5).map_err(|error| error.to_string())?;
                if kind != "cache"
                    || (tag && (lane == "claude" || lane == "codex"))
                    || !(phase == 0
                        || (phase == 1
                            && snapshot.mode == SelectionMode::Report
                            && matches!(lane, "pi" | "kimchi" | "senpi" | "omp")))
                    || !matches!(
                        row.get_ref(6).map_err(|error| error.to_string())?,
                        rusqlite::types::ValueRef::Null
                    )
                    || !matches!(
                        row.get_ref(7).map_err(|error| error.to_string())?,
                        rusqlite::types::ValueRef::Null
                    )
                {
                    return Ok(None);
                }
                let client = match lane {
                    "claude" if !headless => ClientId::Claude,
                    "codex" => ClientId::Codex,
                    "pi" => ClientId::Pi,
                    "kimchi" => ClientId::Kimchi,
                    "senpi" => ClientId::Senpi,
                    "omp" => ClientId::Omp,
                    _ => return Ok(None),
                };
                if rows.next().map_err(|error| error.to_string())?.is_some() {
                    return Ok(None);
                }
                sources.push(IncrementalSource {
                    client,
                    index,
                    headless,
                    tag,
                });
            }
            Ok(Some(sources))
        })();
        let released = self.unpin();
        released?;
        result
    }

    /// Carry immutable descriptors, not historical message payloads. Recheck
    /// admission under the write transaction before copying the base scope.
    pub(crate) fn copy_scope_sources(
        &self,
        previous: &UsageSnapshot,
        next: &UsageSnapshot,
    ) -> Result<bool, String> {
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT current_generation FROM sq_scopes WHERE scope_key=?1",
                [&previous.scope],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        if current != Some(previous.generation)
            || previous.scope != next.scope
            || Self::read_store_epoch(&transaction)? != Some(previous.store_epoch)
        {
            return Ok(false);
        }
        // Full Report ingestion gives unchanged nonempty cache hits phase 0,
        // including sources that were cold in the previous generation. Empty
        // or non-cacheable sources remain misses; observed archives keep their
        // retained authority. Changed sources replace these copied descriptors
        // with the phase selected by the same cache-hit gate as full ingestion.
        self.admit_staging(&transaction)?;
        transaction
            .execute(
                "INSERT INTO sq_scope_sources
             SELECT ?1,s.kind,s.lane,s.source_order,s.path,s.cache_source_id,s.cache_generation,
                    s.generation,s.tag,s.headless,s.workspace,s.workspace_label,
                    CASE WHEN ?3 AND s.kind='cache' AND s.phase=1
                              AND EXISTS(SELECT 1 FROM mc_versions v
                                  WHERE v.source_id=s.cache_source_id
                                    AND v.generation=s.cache_generation
                                    AND v.cacheable!=0 AND v.message_count!=0)
                         THEN 0 ELSE s.phase END
             FROM sq_scope_sources s WHERE s.scope_generation=?2",
                params![
                    next.generation,
                    previous.generation,
                    next.mode == SelectionMode::Report
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }

    /// Pin prior cache descriptors in one commit before reading any sources.
    /// This is opt-in for full discovery, never the incremental-copy path.
    pub(crate) fn seed_cache_descriptors(&self) -> Result<(), String> {
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        let generation = self.admit_staging(&transaction)?;
        transaction
            .execute(
                "UPDATE temp.sq_sources SET seeded_seen=0 WHERE seeded_seen!=0",
                [],
            )
            .map_err(|error| error.to_string())?;
        transaction.execute(
            "INSERT INTO sq_scope_sources(scope_generation,kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase)
             SELECT ?1,s.kind,s.lane,s.source_order,s.path,s.cache_source_id,s.cache_generation,s.generation,s.tag,s.headless,s.workspace,s.workspace_label,s.phase
             FROM sq_scope_sources s
             JOIN sq_scopes owner ON owner.current_generation=s.scope_generation
             JOIN sq_scope_generations staged ON staged.scope_id=owner.id
             WHERE staged.id=?1 AND s.kind='cache'",
            [generation],
        ).map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        self.seeded_cache_descriptors.set(true);
        Ok(())
    }

    fn mark_cache_seen(&self, lane: &str, index: usize) -> Result<(), String> {
        if self.seeded_cache_descriptors.get() {
            self.db
                .prepare_cached(
                    "INSERT INTO sq_sources(lane,source_order,miss,seeded_seen) VALUES(?1,?2,0,1)
                 ON CONFLICT(lane,source_order) DO UPDATE SET seeded_seen=1",
                )
                .map_err(|error| error.to_string())?
                .execute(params![lane, index as i64])
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Unvisited inherited descriptors are not evidence of present sources.
    /// Remove them before absent-source recovery applies its authority policy.
    pub(crate) fn finish_seeded_cache_descriptors(&self) -> Result<(), String> {
        if !self.seeded_cache_descriptors.get() {
            return Ok(());
        }
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        let generation = self.admit_staging(&transaction)?;
        transaction
            .execute(
                "DELETE FROM sq_scope_sources AS s WHERE s.scope_generation=?1 AND s.kind='cache'
             AND NOT EXISTS(SELECT 1 FROM sq_sources seen WHERE seen.lane=s.lane
                            AND seen.source_order=s.source_order AND seen.seeded_seen=1)",
                [generation],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        self.seeded_cache_descriptors.set(false);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_cache(
        &self,
        path: &Path,
        metadata: &CachedSourceMetadata,
        lane: &str,
        index: usize,
        phase: i32,
        tag: bool,
        headless: bool,
    ) -> Result<(), String> {
        let admission =
            rusqlite::Transaction::new_unchecked(&self.db, rusqlite::TransactionBehavior::Deferred)
                .map_err(|error| error.to_string())?;
        let generation = self.admit_staging(&admission)?;
        let encoded = bincode::options()
            .serialize(&crate::message_cache::CachedPath::from_path(path))
            .map_err(|error| error.to_string())?;
        let identical: bool = admission
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM sq_scope_sources s
             JOIN mc_versions v ON v.source_id=s.cache_source_id AND v.generation=s.cache_generation
             WHERE s.scope_generation=?1 AND s.kind='cache' AND s.lane=?2 AND s.source_order=?3
               AND s.path=?4 AND s.cache_source_id=?5 AND s.cache_generation=?6
               AND s.phase=?7 AND s.tag=?8 AND s.headless=?9
               AND s.generation IS NULL AND s.workspace IS NULL AND s.workspace_label IS NULL
               AND NOT EXISTS(SELECT 1 FROM sq_scope_sources observed
                   WHERE observed.scope_generation=s.scope_generation AND observed.lane=s.lane
                     AND observed.source_order=s.source_order
                     AND observed.kind IN ('observed','observed-legacy')))",
            )
            .map_err(|error| error.to_string())?
            .query_row(
                params![
                    generation,
                    lane,
                    index as i64,
                    encoded,
                    metadata.source_id,
                    metadata.generation,
                    phase,
                    tag,
                    headless
                ],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if identical {
            admission.commit().map_err(|error| error.to_string())?;
            return self.mark_cache_seen(lane, index);
        }
        drop(admission);
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        self.register_cache_on(
            &transaction,
            path,
            metadata,
            lane,
            index,
            phase,
            tag,
            headless,
        )?;
        transaction.commit().map_err(|error| error.to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn register_cache_on(
        &self,
        db: &Connection,
        path: &Path,
        metadata: &CachedSourceMetadata,
        lane: &str,
        index: usize,
        phase: i32,
        tag: bool,
        headless: bool,
    ) -> Result<(), String> {
        if db.is_autocommit() {
            return Err("cache registration requires a transaction".into());
        }
        let generation = self.admit_staging(db)?;
        let path = bincode::options()
            .serialize(&crate::message_cache::CachedPath::from_path(path))
            .map_err(|error| error.to_string())?;
        let available: bool = db
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM mc_versions WHERE source_id=?1 AND generation=?2)",
            )
            .map_err(|error| error.to_string())?
            .query_row(params![metadata.source_id, metadata.generation], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        if !available {
            return Err(
                "cache generation changed before scope registration; synchronize and reissue"
                    .into(),
            );
        }
        db.prepare_cached(
            "DELETE FROM sq_scope_sources WHERE scope_generation=?1 AND kind='cache' AND lane=?2 AND source_order=?3",
        ).map_err(|error| error.to_string())?
            .execute(params![generation,lane,index as i64])
            .map_err(|error| error.to_string())?;
        db.prepare_cached(
            "INSERT INTO sq_scope_sources(scope_generation,kind,lane,source_order,path,cache_source_id,cache_generation,tag,headless,phase)
             VALUES(?1,'cache',?2,?3,?4,?5,?6,?7,?8,?9)",
        ).map_err(|error| error.to_string())?
            .execute(params![generation,lane,index as i64,path,metadata.source_id,metadata.generation,tag,headless,phase])
            .map_err(|error| error.to_string())?;
        db.execute("DELETE FROM sq_scope_sources WHERE scope_generation=?1 AND kind IN ('observed','observed-legacy') AND lane=?2 AND source_order=?3",params![generation,lane,index as i64]).map_err(|error|error.to_string())?;
        self.mark_cache_seen(lane, index)?;
        Ok(())
    }

    pub(crate) fn observe_cache(
        &self,
        path: &Path,
        metadata: &CachedSourceMetadata,
        lane: &str,
        index: usize,
        tag: bool,
    ) -> Result<(), String> {
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        self.observe_cache_on(&transaction, path, metadata, lane, index, tag)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    fn observe_cache_on(
        &self,
        db: &Connection,
        path: &Path,
        metadata: &CachedSourceMetadata,
        lane: &str,
        index: usize,
        tag: bool,
    ) -> Result<(), String> {
        if db.is_autocommit() {
            return Err("retained cache registration requires a transaction".into());
        }
        let generation = self.admit_staging(db)?;
        let path = bincode::options()
            .serialize(&crate::message_cache::CachedPath::from_path(path))
            .map_err(|error| error.to_string())?;
        for (index_kind, metadata) in std::iter::once(metadata)
            .chain(metadata.legacy_history.as_deref())
            .enumerate()
        {
            let available: bool = db
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM mc_versions WHERE source_id=?1 AND generation=?2)",
                )
                .map_err(|error| error.to_string())?
                .query_row(params![metadata.source_id, metadata.generation], |row| {
                    row.get(0)
                })
                .map_err(|error| error.to_string())?;
            if !available {
                return Err(
                    "observed retained generation unavailable; synchronize and reissue".into(),
                );
            }
            let kind = if index_kind == 0 {
                "observed"
            } else {
                "observed-legacy"
            };
            db.prepare_cached(
                "DELETE FROM sq_scope_sources WHERE scope_generation=?1 AND kind=?2 AND lane=?3 AND source_order=?4",
            ).map_err(|error| error.to_string())?
                .execute(params![generation,kind,lane,index as i64])
                .map_err(|error| error.to_string())?;
            db.prepare_cached(
                "INSERT INTO sq_scope_sources(scope_generation,kind,lane,source_order,path,cache_source_id,cache_generation,tag,headless)
                 VALUES(?1,?7,?2,?3,?4,?5,?6,?8,0)",
            ).map_err(|error| error.to_string())?
                .execute(params![generation,lane,index as i64,path,metadata.source_id,metadata.generation,kind,tag])
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn collect_abandoned_scope_headers(&self) {
        let _=self.db.execute_batch("DELETE FROM sq_scopes WHERE current_generation IS NULL AND NOT EXISTS(SELECT 1 FROM sq_scope_generations g WHERE g.scope_id=sq_scopes.id);");
    }

    pub(crate) fn register_opencode(
        &self,
        db: &Connection,
        path: &Path,
        sqlite: bool,
        index: usize,
        key: i64,
    ) -> Result<(), String> {
        let generation = self.admit_staging(db)?;
        let path = bincode::options()
            .serialize(&crate::message_cache::CachedPath::from_path(path))
            .map_err(|error| error.to_string())?;
        db.execute("INSERT INTO sq_scope_sources(scope_generation,kind,lane,source_order,path,generation,tag,headless) VALUES(?1,?2,'opencode',?3,?4,?5,0,0)",params![generation,if sqlite {"opencode-sqlite"}else{"opencode-json"},index as i64,path,key]).map(|_|()).map_err(|error|error.to_string())
    }

    pub(crate) fn set_workspace_reference(
        &self,
        lane: &str,
        index: usize,
        key: &Option<String>,
        label: &Option<String>,
    ) -> Result<(), String> {
        if let Some(generation) = self.pending_scope.get() {
            self.db.execute("UPDATE sq_scope_sources SET workspace=?4,workspace_label=?5 WHERE scope_generation=?1 AND lane=?2 AND source_order=?3",params![generation,lane,index as i64,bincode::serialize(key).map_err(|error|error.to_string())?,bincode::serialize(label).map_err(|error|error.to_string())?]).map_err(|error|error.to_string())?;
        }
        Ok(())
    }

    pub(crate) fn publish(&self, snapshot: &mut UsageSnapshot) -> Result<(), String> {
        self.publish_inner(snapshot, false)
    }

    pub(crate) fn publish_prepared(&self, snapshot: &mut UsageSnapshot) -> Result<(), String> {
        self.publish_inner(snapshot, true)
    }

    fn publish_inner(&self, snapshot: &mut UsageSnapshot, prepare: bool) -> Result<(), String> {
        self.finish_seeded_cache_descriptors()?;
        self.db
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| error.to_string())?;
        let result = (|| {
            self.admit_staging(&self.db)?;
            crate::validate_scope_source_generations(&self.db, snapshot.generation)?;
            let previous: Option<i64> = self
                .db
                .query_row(
                    "SELECT current_generation FROM sq_scopes WHERE scope_key=?1",
                    [&snapshot.scope],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if let Some(previous) = previous {
                if previous > snapshot.generation {
                    return Err(
                        "a newer usage scope generation already published; synchronize and reissue"
                            .into(),
                    );
                }
                let projection="kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase";
                let sql=format!("SELECT NOT EXISTS(SELECT {projection} FROM sq_scope_sources WHERE scope_generation=?1 EXCEPT SELECT {projection} FROM sq_scope_sources WHERE scope_generation=?2) AND NOT EXISTS(SELECT {projection} FROM sq_scope_sources WHERE scope_generation=?2 EXCEPT SELECT {projection} FROM sq_scope_sources WHERE scope_generation=?1)");
                let same: bool = self
                    .db
                    .query_row(&sql, params![previous, snapshot.generation], |row| {
                        row.get(0)
                    })
                    .map_err(|error| error.to_string())?;
                if same {
                    self.db
                        .execute(
                            "DELETE FROM sq_scope_sources WHERE scope_generation=?1",
                            [snapshot.generation],
                        )
                        .map_err(|error| error.to_string())?;
                    self.db
                        .execute(
                            "DELETE FROM sq_scope_generations WHERE id=?1",
                            [snapshot.generation],
                        )
                        .map_err(|error| error.to_string())?;
                    snapshot.generation = previous;
                }
                if previous != snapshot.generation {
                    crate::message_cache::reconcile_missing_sources_on(
                        &self.db,
                        previous,
                        snapshot.generation,
                    )?;
                    crate::usage_store::reconcile_missing_opencode_sources_on(
                        &self.db,
                        previous,
                        snapshot.generation,
                    )?;
                }
            }
            if self.pending_scope.get() == Some(snapshot.generation) {
                self.db
                    .execute(
                        "UPDATE sq_scope_generations SET complete=1 WHERE id=?1",
                        [snapshot.generation],
                    )
                    .map_err(|error| error.to_string())?;
                self.db
                    .execute(
                        "UPDATE sq_scopes SET current_generation=?2 WHERE scope_key=?1",
                        params![snapshot.scope, snapshot.generation],
                    )
                    .map_err(|error| error.to_string())?;
            }
            if snapshot.mode == SelectionMode::LocalClients {
                self.build_local_index(snapshot)?;
            } else {
                self.build_report_index_for_publish(snapshot)?;
            }
            // Dynamic origins are recorded by the ingest owner before publish
            // (staged rows visible); completion itself is this COMMIT.
            self.db.execute_batch("DELETE FROM sq_scopes WHERE current_generation IS NOT NULL AND id NOT IN (SELECT id FROM sq_scopes WHERE current_generation IS NOT NULL ORDER BY current_generation DESC,id DESC LIMIT 32);").map_err(|error|error.to_string())?;
            // Discover victims from generation headers before removing them.
            // Incomplete generations (including partially built indexes) belong
            // to their staging owner and must survive another scope's publish.
            let stale = self.db.prepare(
                "SELECT g.id FROM sq_scope_generations g WHERE g.complete=1 AND NOT EXISTS(SELECT 1 FROM sq_scopes s WHERE s.current_generation=g.id)
                 UNION SELECT i.scope_generation FROM sq_indexes i WHERE NOT EXISTS(SELECT 1 FROM sq_scope_generations g WHERE g.id=i.scope_generation)",
            ).map_err(|error| error.to_string())?
                .query_map([], |row| row.get::<_, i64>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            for generation in stale {
                let owner: Option<i64> = self
                    .db
                    .query_row(
                        "SELECT decision_generation FROM sq_indexes WHERE scope_generation=?1",
                        [generation],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                for sql in [
                    "DELETE FROM sq_indexes WHERE scope_generation=?1",
                    "DELETE FROM sq_scope_sources WHERE scope_generation=?1",
                    "DELETE FROM sq_scope_generations WHERE id=?1",
                ] {
                    self.db
                        .execute(sql, [generation])
                        .map_err(|error| error.to_string())?;
                }
                for candidate in [Some(generation), owner.filter(|&id| id != generation)]
                    .into_iter()
                    .flatten()
                {
                    let retained: bool = self
                        .db
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE decision_generation=?1)",
                            [candidate],
                            |row| row.get(0),
                        )
                        .map_err(|error| error.to_string())?;
                    if !retained {
                        self.db
                            .execute(
                                "DELETE FROM sq_decisions WHERE scope_generation=?1",
                                [candidate],
                            )
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            crate::usage_store::discard_unreferenced_opencode_generations(&self.db)?;
            crate::message_cache::collect_unreferenced_versions_on(&self.db)?;
            if prepare {
                self.prepare_query(snapshot, UsageQuery::default())?;
            }
            self.db
                .execute_batch("COMMIT")
                .map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = self.db.execute_batch("ROLLBACK");
        } else {
            self.pending_scope.set(None);
        }
        result
    }

    pub(crate) fn pin(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        if self.read_pinned.get() {
            return Err("usage query already has a pinned reader".into());
        }
        self.db
            .execute_batch("BEGIN")
            .map_err(|error| error.to_string())?;
        let result = (|| {
            if Self::read_store_epoch(&self.db)? != Some(snapshot.store_epoch) {
                return Err(
                    "usage snapshot belongs to a replaced store; synchronize and reissue".into(),
                );
            }
            let current: Option<i64> = self
                .db
                .query_row(
                    "SELECT current_generation FROM sq_scopes WHERE scope_key=?1",
                    [&snapshot.scope],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .flatten();
            if current != Some(snapshot.generation) {
                return Err("usage snapshot is stale or evicted; synchronize and reissue".into());
            }
            self.read_pinned.set(true);
            Ok(())
        })();
        if result.is_err() {
            let _ = self.db.execute_batch("ROLLBACK");
        }
        result
    }

    pub(crate) fn unpin(&self) -> Result<(), String> {
        self.db
            .execute_batch("COMMIT")
            .map_err(|error| error.to_string())?;
        self.read_pinned.set(false);
        Ok(())
    }

    pub(crate) fn enrich_grok_metadata(&self) -> Result<(), String> {
        self.db.execute_batch("UPDATE sq_rows AS u SET model=(SELECT MIN(l.model) FROM sq_rows l WHERE l.lane='grok' AND l.session=u.session AND (l.dedup IS NULL OR l.dedup NOT LIKE 'grok-unified:%') AND l.model!='grok-unknown')
            WHERE u.lane='grok' AND u.dedup LIKE 'grok-unified:%' AND u.model='grok-unknown'
            AND (SELECT COUNT(DISTINCT l.model) FROM sq_rows l WHERE l.lane='grok' AND l.session=u.session AND (l.dedup IS NULL OR l.dedup NOT LIKE 'grok-unified:%') AND l.model!='grok-unknown')=1;").map_err(|error|error.to_string())
    }

    #[cfg(test)]
    pub(crate) fn replay_snapshot(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        self.replay_cache_rows(snapshot, true, None)
    }
    fn check_sources_complete(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        crate::message_cache::install_logical_views_on(&self.db)?;
        let version = self.data_version()?;
        let witness = (snapshot.generation, version, self.db.total_changes());
        if self.complete_sources.get() == Some(witness) {
            return Ok(());
        }
        #[cfg(test)]
        self.completeness_checks
            .set(self.completeness_checks.get() + 1);
        crate::message_cache::validate_scope_metadata_on(&self.db, snapshot.generation)?;
        let missing: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN mc_versions v ON v.source_id=s.cache_source_id AND v.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND (v.source_id IS NULL OR v.message_count!=(SELECT COUNT(*) FROM mc_logical_storage st JOIN mc_rows r INDEXED BY mc_rows_time ON r.source_id=st.source_id AND r.generation=st.row_generation AND r.ordinal<st.row_count WHERE st.source_id=s.cache_source_id AND st.generation=s.cache_generation)))",
            [snapshot.generation], |row| row.get(0),
        ).map_err(|error| error.to_string())?;
        if missing {
            self.complete_sources.set(None);
            return Err("usage snapshot source generation unavailable or incomplete".into());
        }
        self.complete_sources.set(Some(witness));
        Ok(())
    }
    /// Called only after the private output driver has finished its TEMP writes.
    pub(crate) fn finish_query_output(&self) {
        if let Some((generation, version, _)) = self.complete_sources.get() {
            self.complete_sources
                .set(Some((generation, version, self.db.total_changes())));
        }
    }

    fn replay_cache_rows(
        &self,
        snapshot: &UsageSnapshot,
        hydrate: bool,
        range: Option<UsageQuery>,
    ) -> Result<(), String> {
        crate::message_cache::install_logical_views_on(&self.db)?;
        self.check_sources_complete(snapshot)?;
        let recent_join = if let Some(range) = range {
            self.prepare_recent_rows(snapshot, range)?;
            " JOIN sq_recent_rows wanted ON wanted.kind=s.kind AND wanted.lane=s.lane AND wanted.source_order=s.source_order AND wanted.ordinal=r.ordinal"
        } else {
            ""
        };
        self.db.execute(&format!("INSERT INTO sq_rows(lane,source_order,phase,retained,global_storage,session,workspace,dedup,model,workspace_label,timestamp,payload,payload_digest,client,provider,headless,cache_source_id,cache_generation,cache_ordinal,cache_format)
            SELECT s.lane,s.source_order,CASE WHEN r.retained!=0 THEN 2 ELSE s.phase END,r.retained,s.tag,r.session_id,COALESCE(s.workspace,r.workspace),r.dedup_key,r.model_id,COALESCE(s.workspace_label,r.workspace_label),r.timestamp,CASE WHEN ?2 THEN r.message ELSE NULL END,CASE WHEN ?2 THEN r.digest ELSE NULL END,r.client,r.provider_id,s.headless,r.source_id,r.generation,r.ordinal,r.format_version
            FROM sq_scope_sources s JOIN mc_logical_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation {recent_join} WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') ORDER BY s.lane,s.phase,s.source_order,r.ordinal"),params![snapshot.generation, hydrate]).map(|_|()).map_err(|error|error.to_string())
    }

    fn prepare_recent_rows(
        &self,
        snapshot: &UsageSnapshot,
        query: UsageQuery,
    ) -> Result<(), String> {
        self.db
            .execute_batch("DELETE FROM sq_recent_rows; DELETE FROM sq_recent_keys;")
            .map_err(|error| error.to_string())?;
        let mut gate = String::from(
            "s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy')",
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&snapshot.generation];
        if let Some(from) = query.from_ms.as_ref() {
            gate.push_str(" AND r.timestamp>=?");
            values.push(from);
        }
        if let Some(until) = query.until_ms.as_ref() {
            gate.push_str(" AND r.timestamp<?");
            values.push(until);
        }
        self.db.execute(&format!(
            "INSERT OR IGNORE INTO sq_recent_rows
             SELECT s.kind,s.lane,s.source_order,r.ordinal FROM sq_scope_sources s
             JOIN mc_logical_storage st ON st.source_id=s.cache_source_id AND st.generation=s.cache_generation
             JOIN mc_rows r INDEXED BY mc_rows_time ON r.source_id=st.source_id AND r.generation=st.row_generation AND r.ordinal<st.row_count
             WHERE {gate} AND COALESCE(r.fallback,0)=0"),
            values.as_slice(),
        ).map_err(|error| error.to_string())?;
        self.db.execute(&format!(
            "INSERT OR IGNORE INTO sq_recent_rows
             SELECT s.kind,s.lane,s.source_order,r.ordinal FROM sq_scope_sources s
             JOIN mc_logical_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation
             WHERE {gate} AND r.fallback!=0"),
            values.as_slice(),
        ).map_err(|error| error.to_string())?;
        // Global dedup keys can cross sessions and can lose to an older row.
        self.db.execute(
            "INSERT OR IGNORE INTO sq_recent_keys
             SELECT s.lane,r.dedup_key FROM sq_recent_rows wanted
             JOIN sq_scope_sources s ON s.scope_generation=?1 AND s.kind=wanted.kind AND s.lane=wanted.lane AND s.source_order=wanted.source_order
             JOIN mc_logical_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation AND r.ordinal=wanted.ordinal
             WHERE r.dedup_key IS NOT NULL AND s.lane NOT IN ('copilot','grok','trae','kiro')
               AND (s.tag!=0 OR s.lane IN ('claude','codex','hermes','gjc'))",
            [snapshot.generation],
        ).map_err(|error| error.to_string())?;
        self.db.execute(
            "INSERT OR IGNORE INTO sq_recent_rows
             SELECT s.kind,s.lane,s.source_order,r.ordinal
             FROM sq_recent_keys k CROSS JOIN mc_rows r CROSS JOIN sq_scope_sources s CROSS JOIN mc_logical_storage st
             WHERE r.dedup_key=k.dedup AND s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy')
               AND s.lane=k.lane AND s.cache_source_id=r.source_id
               AND st.source_id=s.cache_source_id AND st.generation=s.cache_generation
               AND r.generation=st.row_generation AND r.ordinal<st.row_count",
            [snapshot.generation],
        ).map_err(|error| error.to_string())?;
        // ponytail: rare cross-row authority lanes replay whole only when
        // active in this range; refine cohorts if their measured cost warrants it.
        self.db.execute(
            "WITH lanes AS MATERIALIZED (
                 SELECT DISTINCT lane FROM sq_recent_rows WHERE lane IN ('copilot','grok','trae','kiro'))
             INSERT OR IGNORE INTO sq_recent_rows
             SELECT s.kind,s.lane,s.source_order,r.ordinal FROM lanes
             CROSS JOIN sq_scope_sources s CROSS JOIN mc_logical_rows r
             WHERE s.scope_generation=?1 AND s.lane=lanes.lane
               AND r.source_id=s.cache_source_id AND r.generation=s.cache_generation
               AND s.kind IN ('cache','observed','observed-legacy')",
            [snapshot.generation],
        ).map(|_| ()).map_err(|error| error.to_string())
    }
    fn prepare_cache_snapshot(
        &self,
        snapshot: &UsageSnapshot,
        hydrate: bool,
    ) -> Result<(), String> {
        self.indexed.set(false);
        self.report_reused.set(false);
        self.db
            .execute_batch("DELETE FROM sq_rows; DELETE FROM sq_selected; DELETE FROM sq_output;")
            .map_err(|e| e.to_string())?;
        self.replay_cache_rows(snapshot, hydrate, None)
    }
    pub(crate) fn prepare_snapshot(
        &self,
        snapshot: &UsageSnapshot,
        hydrate_aliases: bool,
    ) -> Result<(), String> {
        self.prepare_cache_snapshot(snapshot, !self.canonical_payloads.get())?;
        // Compact queries skip only hydration aliases; authority aliases stay.
        crate::usage_store::prepare_opencode_snapshot_on(
            &self.db,
            snapshot.generation,
            hydrate_aliases,
        )?;
        // Report-only eligibility: baseline LocalClients applied no client
        // filtering before arbitration (its select gate reads eligible only
        // in Report mode), so LocalClients keeps the default eligible=1.
        if snapshot.mode == SelectionMode::Report {
            self.set_client_eligibility(&snapshot.clients)?;
        }
        crate::for_each_usage_lane(snapshot.mode, &mut |lane, _, simple, _| {
            self.select_lane(lane, snapshot.mode, simple)
        })
    }

    pub(crate) fn take_report_reused(&self) -> bool {
        self.report_reused.replace(false)
    }

    fn decision_generation(&self, generation: i64) -> Result<i64, String> {
        self.db.query_row(
            "SELECT decision_generation FROM sq_indexes WHERE scope_generation=?1 AND decision_generation>0",
            [generation], |row| row.get(0),
        ).map_err(|error| error.to_string())
    }

    fn reuse_decision_index(
        &self,
        snapshot: &UsageSnapshot,
        version: i64,
        opencode: bool,
    ) -> Result<bool, String> {
        let gate = if opencode {
            "kind IN ('opencode-sqlite','opencode-json')"
        } else {
            "kind IN ('cache','observed','observed-legacy') AND lane IN ('claude','codex')"
        };
        let columns = "kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase";
        let exact_sql = format!(
            "SELECT NOT EXISTS(SELECT {columns} FROM sq_scope_sources WHERE scope_generation=?1 AND {gate}
               EXCEPT SELECT {columns} FROM sq_scope_sources WHERE scope_generation=?2 AND {gate})
             AND NOT EXISTS(SELECT {columns} FROM sq_scope_sources WHERE scope_generation=?2 AND {gate}
               EXCEPT SELECT {columns} FROM sq_scope_sources WHERE scope_generation=?1 AND {gate})"
        );
        let mut statement = self
            .db
            .prepare_cached(
                "SELECT g.id,i.decision_generation FROM sq_scope_generations g
             JOIN sq_indexes i ON i.scope_generation=g.id
             WHERE g.scope_id=(SELECT scope_id FROM sq_scope_generations WHERE id=?1)
               AND g.id!=?1 AND g.complete=1 AND i.version=?2 ORDER BY g.id DESC",
            )
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query(params![snapshot.generation, version])
            .map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let previous: i64 = row.get(0).map_err(|error| error.to_string())?;
            let owner: i64 = row.get(1).map_err(|error| error.to_string())?;
            let exact: bool = self
                .db
                .query_row(&exact_sql, params![snapshot.generation, previous], |row| {
                    row.get(0)
                })
                .map_err(|error| error.to_string())?;
            if !exact {
                continue;
            }
            let dangling_sql = if opencode {
                "SELECT EXISTS(SELECT 1 FROM sq_decisions d LEFT JOIN oc_messages m ON m.source=d.oc_source AND m.ordinal=d.oc_ordinal WHERE d.scope_generation=?1 AND d.oc_source IS NOT NULL AND m.source IS NULL)"
            } else {
                "SELECT EXISTS(SELECT 1 FROM sq_decisions d LEFT JOIN mc_logical_storage st ON st.source_id=d.cache_source_id AND st.generation=d.cache_generation LEFT JOIN mc_rows r ON r.source_id=st.source_id AND r.generation=st.row_generation AND r.ordinal=d.cache_ordinal AND r.ordinal<st.row_count WHERE d.scope_generation=?1 AND d.cache_source_id IS NOT NULL AND r.ordinal IS NULL)"
            };
            let dangling: bool = self
                .db
                .query_row(dangling_sql, [owner], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if dangling {
                continue;
            }
            self.db.execute(
                "INSERT INTO sq_indexes(scope_generation,version,decision_generation) VALUES(?1,?2,?3)",
                params![snapshot.generation, version, owner],
            ).map_err(|error| error.to_string())?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Exact-generation Report index readiness, including empty winner sets.
    /// Older stores prepare fully until the next capture builds the index.
    /// LocalClients ownership is never borrowed.
    fn report_index_ready(&self, snapshot: &UsageSnapshot) -> Result<bool, String> {
        if snapshot.mode != SelectionMode::Report
            || snapshot.modified_after.is_some()
            || snapshot.selection_ignores_filter
            || self.indexed.get()
        {
            return Ok(false);
        }
        let ready: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE scope_generation=?1 AND version=?2)",
                rusqlite::params![snapshot.generation, REPORT_INDEX_VERSION],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        Ok(ready)
    }

    /// Persist this generation's own selected Claude/Codex winner references.
    /// Runs inside the successful publication transaction, before GC, for
    /// every eligible Report publication regardless of the prepare flag, so
    /// ordinary and targeted captures share it with no per-caller wiring.
    /// Replays only C/C canonical sources (completeness-gated, losers and
    /// zero-winner sources included), applies snapshot client eligibility
    /// before arbitration, then selects just those two lanes. The replay is
    /// metadata-only: arbitration and the durable decision rows read only
    /// ordering/attribution columns, so message/digest BLOBs stay in durable
    /// facts and never enter publish scratch. SQL-only; idempotent on
    /// collapsed re-publish; never touches LocalClients rows.
    fn build_report_index_for_publish(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        if snapshot.mode != SelectionMode::Report
            || snapshot.modified_after.is_some()
            || snapshot.selection_ignores_filter
        {
            return Ok(());
        }
        let built: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE scope_generation=?1 AND version=?2)",
                rusqlite::params![snapshot.generation, REPORT_INDEX_VERSION],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if built {
            return Ok(());
        }
        self.check_report_side_complete(snapshot, true)?;
        if self.reuse_decision_index(snapshot, REPORT_INDEX_VERSION, false)? {
            return Ok(());
        }
        self.db
            .execute_batch("DELETE FROM sq_rows; DELETE FROM sq_selected; DELETE FROM sq_output;")
            .map_err(|error| error.to_string())?;
        self.replay_report_other_lanes(snapshot, true)?;
        // The publish scratch holds only Claude/Codex rows, so the grok
        // cohort fixup has no rows to read; eligibility is the same
        // client match the fold applies, minus that no-op fixup.
        self.set_client_eligibility_without_grok(&snapshot.clients)?;
        self.select_lane("claude", snapshot.mode, false)?;
        self.select_lane("codex", snapshot.mode, false)?;
        self.db
            .execute(
                "INSERT OR IGNORE INTO sq_decisions(scope_generation,rank,lane,timestamp,cache_source_id,cache_generation,cache_ordinal,cache_format,retained,global_storage,workspace,workspace_label,model,headless)
                 SELECT ?1,ROW_NUMBER() OVER(ORDER BY s.rank),r.lane,r.timestamp,r.cache_source_id,r.cache_generation,r.cache_ordinal,r.cache_format,r.retained,r.global_storage,NULLIF(r.workspace,c.workspace),NULLIF(r.workspace_label,c.workspace_label),NULLIF(r.model,c.model_id),r.headless
                 FROM sq_rows r JOIN sq_selected s ON s.id=r.id
                 LEFT JOIN mc_logical_storage st ON st.source_id=r.cache_source_id AND st.generation=r.cache_generation
                 LEFT JOIN mc_rows c ON c.source_id=st.source_id AND c.generation=st.row_generation AND c.ordinal=r.cache_ordinal AND c.ordinal<st.row_count
                 WHERE r.lane IN ('claude','codex') AND r.cache_source_id IS NOT NULL ORDER BY s.rank",
                [snapshot.generation],
            )
            .map_err(|error| error.to_string())?;
        self.db
            .execute(
                "INSERT OR IGNORE INTO sq_indexes(scope_generation,version,decision_generation) VALUES(?1,?2,?1)",
                rusqlite::params![snapshot.generation, REPORT_INDEX_VERSION],
            )
            .map_err(|error| error.to_string())?;
        // Cold publication keeps only winner references: release the
        // arbitration scratch so no history-sized rows stay resident on the
        // publishing connection. Later reads replay from durable facts
        // (indexed winners or full preparation), which prepare clears first.
        self.db
            .execute_batch("DELETE FROM sq_rows; DELETE FROM sq_selected; DELETE FROM sq_output;")
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Completeness gate for one side of the lane split, without replaying
    /// any payloads: every referenced canonical source on this side —
    /// including duplicate losers and zero-winner sources — must still hold
    /// its exact row count, else the query errors whole before any callback.
    fn check_report_side_complete(
        &self,
        snapshot: &UsageSnapshot,
        only_cc: bool,
    ) -> Result<(), String> {
        crate::message_cache::install_logical_views_on(&self.db)?;
        let gate = "SELECT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN mc_versions v ON v.source_id=s.cache_source_id AND v.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND (s.lane IN ('claude','codex'))=?2 AND (v.source_id IS NULL OR v.message_count!=(SELECT COUNT(*) FROM mc_logical_storage st JOIN mc_rows r INDEXED BY mc_rows_time ON r.source_id=st.source_id AND r.generation=st.row_generation AND r.ordinal<st.row_count WHERE st.source_id=s.cache_source_id AND st.generation=s.cache_generation)))";
        let missing: bool = self
            .db
            .query_row(gate, params![snapshot.generation, only_cc], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        if missing {
            return Err("usage snapshot source generation unavailable or incomplete".into());
        }
        Ok(())
    }

    /// Arbitration needs metadata only. Trusted reports resolve cached payloads
    /// under their admitted read pin; detached public visitors still copy bytes.
    fn replay_report_other_lanes(
        &self,
        snapshot: &UsageSnapshot,
        only_cc: bool,
    ) -> Result<(), String> {
        self.check_report_side_complete(snapshot, only_cc)?;
        self.db
            .execute(
                "INSERT INTO sq_rows(lane,source_order,phase,retained,global_storage,session,workspace,dedup,model,workspace_label,timestamp,payload,payload_digest,client,provider,headless,cache_source_id,cache_generation,cache_ordinal,cache_format)
                SELECT s.lane,s.source_order,CASE WHEN r.retained!=0 THEN 2 ELSE s.phase END,r.retained,s.tag,r.session_id,COALESCE(s.workspace,r.workspace),r.dedup_key,r.model_id,COALESCE(s.workspace_label,r.workspace_label),r.timestamp,CASE WHEN ?2 OR ?3 THEN NULL ELSE r.message END,CASE WHEN ?2 OR ?3 THEN NULL ELSE r.digest END,r.client,r.provider_id,s.headless,r.source_id,r.generation,r.ordinal,r.format_version
                FROM sq_scope_sources s JOIN mc_logical_rows r ON r.source_id=s.cache_source_id AND r.generation=s.cache_generation WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy') AND (s.lane IN ('claude','codex'))=?2 ORDER BY s.lane,s.phase,s.source_order,r.ordinal",
                params![snapshot.generation, only_cc, self.canonical_payloads.get()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Copy this snapshot's owned winner group using current captured sources. Completeness
    /// reuses the same C/C gate as publication (losers and zero-winner
    /// sources included) without replaying their payloads; the winner-count
    /// match is additional, and a short copy falls back to full preparation,
    /// which errors whole on corruption. SQL only; no history-sized Vec.
    fn replay_report_winners(&self, snapshot: &UsageSnapshot) -> Result<bool, String> {
        self.check_report_side_complete(snapshot, true)?;
        let owner = self.decision_generation(snapshot.generation)?;
        let expected: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM sq_decisions WHERE scope_generation=?1 AND lane IN ('claude','codex') AND oc_source IS NULL",
                [owner],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        self.db
            .execute("INSERT INTO sq_rows(lane,source_order,phase,retained,global_storage,session,workspace,dedup,model,workspace_label,timestamp,payload,payload_digest,client,provider,headless,cache_source_id,cache_generation,cache_ordinal,cache_format) SELECT s.lane,s.source_order,CASE WHEN r.retained!=0 THEN 2 ELSE s.phase END,r.retained,s.tag,r.session_id,COALESCE(s.workspace,r.workspace),r.dedup_key,r.model_id,COALESCE(s.workspace_label,r.workspace_label),r.timestamp,CASE WHEN ?3 THEN NULL ELSE r.message END,CASE WHEN ?3 THEN NULL ELSE r.digest END,r.client,r.provider_id,s.headless,r.source_id,r.generation,r.ordinal,r.format_version FROM sq_decisions d JOIN mc_logical_rows r ON r.source_id=d.cache_source_id AND r.generation=d.cache_generation AND r.ordinal=d.cache_ordinal JOIN sq_scope_sources s ON s.scope_generation=?2 AND s.kind IN ('cache','observed','observed-legacy') AND s.lane=d.lane AND s.cache_source_id=r.source_id AND s.cache_generation=r.generation WHERE d.scope_generation=?1 AND d.lane IN ('claude','codex') AND d.oc_source IS NULL ORDER BY d.rank", params![owner, snapshot.generation, self.canonical_payloads.get()])
            .map_err(|error| error.to_string())?;
        let copied: i64 = self
            .db
            .query_row(
                "SELECT COUNT(*) FROM sq_rows WHERE lane IN ('claude','codex')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        Ok(copied == expected)
    }

    fn build_local_index(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        let ready: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE scope_generation=?1 AND version=3)",
                [snapshot.generation],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if ready {
            return Ok(());
        }
        crate::message_cache::install_logical_views_on(&self.db)?;
        crate::message_cache::validate_scope_metadata_on(&self.db, snapshot.generation)?;
        self.check_report_side_complete(snapshot, true)?;
        self.check_report_side_complete(snapshot, false)?;
        if self.reuse_decision_index(snapshot, 3, true)? {
            return Ok(());
        }
        crate::usage_store::prepare_opencode_snapshot_on(&self.db, snapshot.generation, false)?;
        crate::usage_store::persist_local_opencode_decisions(&self.db, snapshot.generation)?;
        self.db.execute(
            "INSERT INTO sq_indexes(scope_generation,version,decision_generation) VALUES(?1,3,?1)",
            [snapshot.generation],
        ).map(|_| ()).map_err(|error| error.to_string())
    }

    fn prepare_local_grok_metadata(&self) -> Result<(), String> {
        self.enrich_grok_metadata()?;
        let none =
            bincode::serialize(&Option::<String>::None).map_err(|error| error.to_string())?;
        self.db.execute("UPDATE sq_rows AS u SET
            workspace=(SELECT l.workspace FROM sq_rows l WHERE l.lane='grok' AND l.session=u.session AND (l.dedup IS NULL OR l.dedup NOT LIKE 'grok-unified:%') AND (l.workspace!=?1 OR l.workspace_label!=?1) LIMIT 1),
            workspace_label=(SELECT l.workspace_label FROM sq_rows l WHERE l.lane='grok' AND l.session=u.session AND (l.dedup IS NULL OR l.dedup NOT LIKE 'grok-unified:%') AND (l.workspace!=?1 OR l.workspace_label!=?1) LIMIT 1)
            WHERE u.lane='grok' AND u.dedup LIKE 'grok-unified:%' AND u.workspace=?1 AND u.workspace_label=?1
            AND (SELECT COUNT(*) FROM (SELECT DISTINCT l.workspace,l.workspace_label FROM sq_rows l WHERE l.lane='grok' AND l.session=u.session AND (l.dedup IS NULL OR l.dedup NOT LIKE 'grok-unified:%') AND (l.workspace!=?1 OR l.workspace_label!=?1)))=1", [&none])
            .map(|_| ()).map_err(|error| error.to_string())
    }

    pub(crate) fn is_indexed(&self) -> bool {
        self.indexed.get()
    }

    pub(crate) fn prepare_trusted_query(
        &self,
        snapshot: &UsageSnapshot,
        query: UsageQuery,
    ) -> Result<(), String> {
        if !self.read_pinned.get() {
            return Err("trusted usage preparation requires a pinned reader".into());
        }
        if snapshot.mode == SelectionMode::Report && !self.canonical_payloads.get() {
            crate::message_cache::install_logical_views_on(&self.db)?;
            self.db
                .execute_batch(
                    "DROP VIEW temp.sq_payload_rows;
                 CREATE TEMP VIEW sq_payload_rows AS
                 SELECT r.id,r.lane,r.source_order,r.phase,r.retained,r.global_storage,
                        r.session,r.workspace,r.dedup,r.model,r.workspace_label,r.timestamp,
                        COALESCE(r.payload,c.message) AS payload,r.client,r.provider,r.headless,
                        COALESCE(r.payload_digest,c.digest) AS payload_digest,r.eligible,
                        r.cache_source_id,r.cache_generation,r.cache_ordinal,r.cache_format
                 FROM sq_rows r LEFT JOIN mc_logical_storage st
                   ON r.payload IS NULL AND st.source_id=r.cache_source_id
                  AND st.generation=r.cache_generation
                 LEFT JOIN main.mc_rows c
                   ON c.source_id=st.source_id AND c.generation=st.row_generation
                  AND c.ordinal=r.cache_ordinal AND c.ordinal<st.row_count;",
                )
                .map_err(|error| error.to_string())?;
            self.canonical_payloads.set(true);
        }
        self.prepare_query(snapshot, query)
    }

    fn admit_payload_reader(&self) -> Result<(), String> {
        if self.canonical_payloads.get() && !self.read_pinned.get() {
            return Err("canonical usage payloads require their admitted reader".into());
        }
        Ok(())
    }

    /// Prepare a range and its complete arbitration cohorts before releasing
    /// the reader. Public callbacks never pin canonical facts against publication.
    pub(crate) fn prepare_query(
        &self,
        snapshot: &UsageSnapshot,
        query: UsageQuery,
    ) -> Result<(), String> {
        let result = self.prepare_query_inner(snapshot, query);
        // Only successful private TEMP work may advance our own-write witness.
        // An external commit always changes the same connection's data_version.
        if result.is_ok() {
            if let Some((generation, version, _)) = self.complete_sources.get() {
                self.complete_sources
                    .set(Some((generation, version, self.db.total_changes())));
            }
        } else {
            self.complete_sources.set(None);
        }
        result
    }

    fn prepare_query_inner(
        &self,
        snapshot: &UsageSnapshot,
        query: UsageQuery,
    ) -> Result<(), String> {
        self.report_reused.set(false);
        if snapshot.mode != SelectionMode::LocalClients {
            crate::message_cache::install_logical_views_on(&self.db)?;
            crate::message_cache::validate_scope_metadata_on(&self.db, snapshot.generation)?;
            // Published C/C groups replay against this snapshot's descriptors.
            // Every other lane still arbitrates fresh before report filtering.
            if self.report_index_ready(snapshot)? {
                self.db
                    .execute_batch(
                        "DELETE FROM sq_rows; DELETE FROM sq_selected; DELETE FROM sq_output;",
                    )
                    .map_err(|error| error.to_string())?;
                self.replay_report_other_lanes(snapshot, false)?;
                if !self.replay_report_winners(snapshot)? {
                    self.report_reused.set(false);
                    return self.prepare_snapshot(snapshot, query.hydrate_opencode_aliases);
                }
                crate::usage_store::prepare_opencode_snapshot_on(
                    &self.db,
                    snapshot.generation,
                    query.hydrate_opencode_aliases,
                )?;
                // Winners replay in durable rank order, so selection rows keep
                // the identical traversal order a full prepare would produce.
                self.db
                    .execute_batch("INSERT INTO sq_selected(id) SELECT id FROM sq_rows WHERE lane IN ('claude','codex') ORDER BY id;")
                    .map_err(|error| error.to_string())?;
                self.report_reused.set(true);
                return Ok(());
            }
            return self.prepare_snapshot(snapshot, query.hydrate_opencode_aliases);
        }
        let ready: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sq_indexes WHERE scope_generation=?1 AND version=3)",
                [snapshot.generation],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !ready {
            return Err(
                "usage snapshot has no complete decision index; synchronize and reissue".into(),
            );
        }
        self.indexed.set(false);
        self.check_sources_complete(snapshot)?;
        self.db
            .execute_batch("DELETE FROM sq_rows; DELETE FROM sq_selected; DELETE FROM sq_output;")
            .map_err(|error| error.to_string())?;
        if let Some((generation, version, _)) = self.complete_sources.get() {
            self.complete_sources
                .set(Some((generation, version, self.db.total_changes())));
        }
        let range = (query.from_ms.is_some() || query.until_ms.is_some()).then_some(query);
        self.replay_cache_rows(snapshot, true, range)?;
        self.prepare_local_grok_metadata()?;
        crate::for_each_usage_lane(snapshot.mode, &mut |lane, _, simple, _| {
            self.select_lane(lane, snapshot.mode, simple)
        })?;
        let owner = self.decision_generation(snapshot.generation)?;
        crate::usage_store::prepare_indexed_opencode_on(&self.db, owner, query)?;
        self.indexed.set(true);
        Ok(())
    }

    pub(crate) fn push(
        &self,
        lane: &str,
        source_order: usize,
        phase: i32,
        retained: bool,
        global_storage: bool,
        message: &UnifiedMessage,
    ) -> Result<(), String> {
        let payload = bincode::serialize(message).map_err(|error| error.to_string())?;
        let workspace =
            bincode::serialize(&message.workspace_key).map_err(|error| error.to_string())?;
        let label =
            bincode::serialize(&message.workspace_label).map_err(|error| error.to_string())?;
        self.db.prepare_cached("INSERT INTO sq_rows(lane,source_order,phase,retained,global_storage,session,workspace,dedup,model,workspace_label,timestamp,payload,client,provider) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)")
            .map_err(|error| error.to_string())?
            .execute(params![lane, source_order as i64, phase, retained, global_storage, message.session_id, workspace, message.dedup_key, message.model_id, label, message.timestamp, payload,message.client,message.provider_id])
            .map(|_| ()).map_err(|error| error.to_string())
    }

    pub(crate) fn classify(&self, lane: &str, index: usize, miss: bool) -> Result<(), String> {
        self.db
            .prepare_cached("INSERT INTO sq_sources(lane,source_order,miss) VALUES(?1,?2,?3) ON CONFLICT(lane,source_order) DO UPDATE SET miss=excluded.miss")
            .map_err(|error| error.to_string())?
            .execute(params![lane, index as i64, miss])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn is_miss(&self, lane: &str, index: usize) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT miss FROM sq_sources WHERE lane=?1 AND source_order=?2",
                params![lane, index as i64],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    fn exists(&self, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<bool, String> {
        self.db
            .prepare_cached(sql)
            .map_err(|error| error.to_string())?
            .query_row(values, |_| Ok(()))
            .optional()
            .map(|row| row.is_some())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn visit(
        &self,
        lane: &str,
        sink: &mut impl FnMut(UnifiedMessage, bool, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        self.visit_sql("SELECT r.payload,r.retained,r.global_storage,r.timestamp,r.workspace,r.workspace_label,r.headless,r.payload_digest,r.model,r.cache_format FROM sq_payload_rows r WHERE r.lane=?1 ORDER BY r.phase,r.source_order,r.id", &[&lane],sink)
    }

    fn visit_sql(
        &self,
        sql: &str,
        values: &[&dyn rusqlite::ToSql],
        sink: &mut impl FnMut(UnifiedMessage, bool, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        self.admit_payload_reader()?;
        let mut statement = self
            .db
            .prepare_cached(sql)
            .map_err(|error| error.to_string())?;
        let mut rows = statement.query(values).map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let message = decode_staged_message(row)?;
            sink(
                message,
                row.get(1).map_err(|error| error.to_string())?,
                row.get(2).map_err(|error| error.to_string())?,
            )?;
        }
        Ok(())
    }

    fn set_client_eligibility_without_grok(&self, clients: &[String]) -> Result<(), String> {
        if clients.is_empty() {
            return Ok(());
        }
        let requested: std::collections::HashSet<&str> =
            clients.iter().map(String::as_str).collect();
        let mut statement = self
            .db
            .prepare_cached("SELECT id,client,model,provider,eligible FROM sq_rows")
            .map_err(|error| error.to_string())?;
        let mut rows = statement.query([]).map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let id: i64 = row.get(0).map_err(|error| error.to_string())?;
            let client = row.get_ref(1).map_err(|error| error.to_string())?;
            let model = row.get_ref(2).map_err(|error| error.to_string())?;
            let provider = row.get_ref(3).map_err(|error| error.to_string())?;
            let eligible = crate::retain_for_requested_clients(
                client.as_str().map_err(|error| error.to_string())?,
                model.as_str().map_err(|error| error.to_string())?,
                provider.as_str().map_err(|error| error.to_string())?,
                &requested,
            );
            let current: i64 = row.get(4).map_err(|error| error.to_string())?;
            if current != i64::from(eligible) {
                self.db
                    .prepare_cached("UPDATE sq_rows SET eligible=?2 WHERE id=?1")
                    .map_err(|error| error.to_string())?
                    .execute(params![id, eligible])
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub(crate) fn set_client_eligibility(&self, clients: &[String]) -> Result<(), String> {
        if self.indexed.get() {
            return Ok(());
        }
        self.enrich_grok_metadata()?;
        if clients.is_empty() {
            // eligible defaults to 1 at insert; a single set-based reset keeps
            // any stale explicit-client marks identical without a per-row loop.
            self.db
                .execute("UPDATE sq_rows SET eligible=1 WHERE eligible!=1", [])
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        self.set_client_eligibility_without_grok(clients)
    }

    pub(crate) fn select_lane(
        &self,
        lane: &str,
        mode: SelectionMode,
        simple: bool,
    ) -> Result<(), String> {
        if self.indexed.get() {
            return Ok(());
        }
        let mut gate = String::from("r.lane=?1");
        if mode == SelectionMode::Report && !matches!(lane, "kiro" | "trae") {
            gate.push_str(" AND r.eligible=1");
        }
        match lane {
            "copilot"=>gate.push_str(" AND (r.global_storage=0 OR NOT EXISTS(SELECT 1 FROM sq_rows o WHERE o.lane='copilot' AND o.global_storage=0 AND o.session=r.session))"),
            "grok"=>gate.push_str(" AND (r.dedup LIKE 'grok-unified:%' OR NOT EXISTS(SELECT 1 FROM sq_rows u WHERE u.lane='grok' AND u.session=r.session AND u.dedup LIKE 'grok-unified:%'))"),
            "trae"=>gate.push_str(" AND NOT EXISTS(SELECT 1 FROM sq_rows n WHERE n.lane='trae' AND n.session=r.session AND (n.timestamp>r.timestamp OR (n.timestamp=r.timestamp AND n.dedup IS NOT NULL AND (r.dedup IS NULL OR n.dedup>r.dedup))))"),
            "kiro"=>gate.push_str(" AND (r.global_storage=0 OR r.dedup IS NULL OR NOT (CASE
                WHEN instr(r.dedup,':globalstorage:exec:')>0 THEN EXISTS(SELECT 1 FROM sq_rows e WHERE e.lane='kiro' AND e.global_storage=1 AND e.workspace=r.workspace AND e.dedup='execution:'||substr(r.dedup,instr(r.dedup,':globalstorage:exec:')+length(':globalstorage:exec:')))
                WHEN r.dedup LIKE '%:workspace-session' THEN EXISTS(SELECT 1 FROM sq_rows e WHERE e.lane='kiro' AND e.global_storage=1 AND e.session=r.session AND e.dedup LIKE 'execution:%')
                WHEN r.dedup LIKE '%:globalstorage' THEN EXISTS(SELECT 1 FROM sq_rows e WHERE e.lane='kiro' AND e.global_storage=1 AND e.workspace=r.workspace AND instr(e.session,'/')=0 AND e.dedup LIKE 'execution:%' AND (r.session=e.session OR substr(r.session,-length(e.session)-1)='/'||e.session))
                ELSE 0 END))"),
            _=>{},
        }
        let dedup = matches!(
            lane,
            "claude" | "codex" | "hermes" | "gjc" | "trae" | "kiro"
        ) || mode == SelectionMode::Report
            && (simple || matches!(lane, "copilot" | "grok" | "gemini" | "antigravity-cli"));
        let local_tag =
            simple && mode != SelectionMode::Report && !matches!(lane, "copilot" | "grok");
        let key = if lane == "trae" {
            "r.session".to_string()
        } else if lane == "antigravity-cli" {
            "r.session||':'||r.dedup".to_string()
        } else {
            "r.dedup".to_string()
        };
        let bypass = if lane == "trae" {
            "0"
        } else if lane == "kiro" || mode == SelectionMode::Report || lane == "claude" {
            "r.dedup IS NULL OR r.dedup=''"
        } else {
            "r.dedup IS NULL"
        };
        let partition = if lane == "kiro" {
            "r.global_storage,CASE WHEN r.global_storage=1 AND r.dedup LIKE 'execution:%' THEN r.workspace ELSE NULL END,r.dedup".to_string()
        } else {
            key
        };
        let order = if lane == "trae" && mode != SelectionMode::Report {
            "session,timestamp"
        } else {
            "phase,source_order,id"
        };
        let sql = if dedup || local_tag {
            let bypass = if local_tag {
                format!("({bypass}) OR r.global_storage=0")
            } else {
                bypass.into()
            };
            // Only Trae's local ordering consumes these projections. Carrying
            // every session string through report arbitration widens both
            // file-backed window/sort streams for the entire history.
            let ordering_columns = if lane == "trae" && mode != SelectionMode::Report {
                ",r.session,r.timestamp"
            } else {
                ""
            };
            format!("INSERT OR IGNORE INTO sq_selected(id) SELECT id FROM (
                SELECT r.id,r.phase,r.source_order{ordering_columns},CASE WHEN {bypass} THEN 1 ELSE ROW_NUMBER() OVER(PARTITION BY CASE WHEN {bypass} THEN r.id ELSE NULL END,{partition} ORDER BY r.phase,r.source_order,r.id) END AS winner FROM sq_rows r WHERE {gate}
                ) WHERE winner=1 ORDER BY {order}")
        } else {
            format!("INSERT OR IGNORE INTO sq_selected(id) SELECT r.id FROM sq_rows r WHERE {gate} ORDER BY r.phase,r.source_order,r.id")
        };
        self.db
            .execute(&sql, [lane])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn visit_selected_lane(
        &self,
        lane: &str,
        query: UsageQuery,
        sink: &mut impl FnMut(UnifiedMessage, bool, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut sql=String::from("SELECT r.payload,r.retained,r.global_storage,r.timestamp,r.workspace,r.workspace_label,r.headless,r.payload_digest,r.model,r.cache_format FROM sq_payload_rows r JOIN sq_selected s ON s.id=r.id WHERE r.lane=?1");
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&lane];
        if let Some(from) = query.from_ms.as_ref() {
            sql.push_str(" AND r.timestamp>=?");
            values.push(from);
        }
        if let Some(until) = query.until_ms.as_ref() {
            sql.push_str(" AND r.timestamp<?");
            values.push(until);
        }
        sql.push_str(" ORDER BY s.rank");
        self.visit_sql(&sql, &values, sink)
    }

    pub(crate) fn buffer_output(&self, message: &UnifiedMessage) -> Result<(), String> {
        // Callbacks may buffer while another read owns rows on this
        // connection, so this stays outside any explicit transaction.
        self.db
            .prepare_cached("INSERT INTO sq_output(timestamp,payload) VALUES(?1,?2)")
            .map_err(|error| error.to_string())?
            .execute(params![
                message.timestamp,
                bincode::serialize(message).map_err(|error| error.to_string())?
            ])
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn visit_output(
        &self,
        query: UsageQuery,
        sink: &mut impl FnMut(&UnifiedMessage),
    ) -> Result<(), String> {
        let sql = if query.chronological {
            "SELECT payload FROM sq_output WHERE (?1 IS NULL OR timestamp>=?1) AND (?2 IS NULL OR timestamp<?2) ORDER BY timestamp,rank"
        } else {
            "SELECT payload FROM sq_output WHERE (?1 IS NULL OR timestamp>=?1) AND (?2 IS NULL OR timestamp<?2) ORDER BY rank"
        };
        let mut statement = self
            .db
            .prepare_cached(sql)
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query(params![query.from_ms, query.until_ms])
            .map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let bytes = row.get_ref(0).map_err(|error| error.to_string())?;
            let bytes = bytes.as_blob().map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    0,
                    row.as_ref().column_name(0).unwrap_or("").to_string(),
                    bytes.data_type(),
                )
                .to_string()
            })?;
            let message = bincode::deserialize(bytes).map_err(|error| error.to_string())?;
            sink(&message);
        }
        Ok(())
    }

    pub(crate) fn grok_select(&self, message: &mut UnifiedMessage) -> Result<bool, String> {
        let unified = message
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.starts_with("grok-unified:"));
        let has_unified = self.exists("SELECT 1 FROM sq_rows WHERE lane='grok' AND session=?1 AND dedup LIKE 'grok-unified:%' LIMIT 1", &[&message.session_id])?;
        if !unified {
            return Ok(!has_unified);
        }
        if message.model_id == "grok-unknown" {
            let mut statement = self.db.prepare_cached("SELECT DISTINCT model FROM sq_rows WHERE lane='grok' AND session=?1 AND (dedup IS NULL OR dedup NOT LIKE 'grok-unified:%') AND model!='grok-unknown' LIMIT 2").map_err(|error| error.to_string())?;
            let models = statement
                .query_map([&message.session_id], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            if models.len() == 1 {
                message.model_id = models[0].clone();
            }
        }
        if message.workspace_key.is_none() && message.workspace_label.is_none() {
            let none =
                bincode::serialize(&Option::<String>::None).map_err(|error| error.to_string())?;
            let mut statement=self.db.prepare_cached("SELECT DISTINCT workspace,workspace_label FROM sq_rows WHERE lane='grok' AND session=?1 AND (dedup IS NULL OR dedup NOT LIKE 'grok-unified:%') AND (workspace!=?2 OR workspace_label!=?2) LIMIT 2").map_err(|error| error.to_string())?;
            let values = statement
                .query_map(params![message.session_id, none], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            if values.len() == 1 {
                message.set_workspace(
                    bincode::deserialize(&values[0].0).map_err(|error| error.to_string())?,
                    bincode::deserialize(&values[0].1).map_err(|error| error.to_string())?,
                );
            }
        }
        Ok(true)
    }
}

impl Drop for QueryState {
    fn drop(&mut self) {
        if let Some(generation) = self.pending_scope.get() {
            let _ = self.db.execute(
                "DELETE FROM sq_scope_sources WHERE scope_generation=?1",
                [generation],
            );
            let _ = self.db.execute(
                "DELETE FROM sq_scope_generations WHERE id=?1 AND complete=0",
                [generation],
            );
            self.collect_abandoned_scope_headers();
        }
        if self.read_pinned.get() {
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
}

/// Carry durable-known sources the scanner no longer discovers into the
/// staged scope. Only sources already stored under a namespace/scope identity
/// compatible with this context are carried (same client namespace; Claude
/// additionally admits an imported scope-0 row alongside the captured home
/// scope): no cross-account, cross-root, or cross-client import. Carried rows
/// keep their stored client/account attribution, so later fold filters still
/// apply exactly.
/// Admission is the captured scan's own `admits_durable_source` (this scan's
/// post-exclusion task roots, native DB candidates independent of existence,
/// the synthetic Octofriend candidate, valid OpenCode DB names, Grok unified
/// candidates, Hermes homes, default Claude mirror/cowork bases) plus the
/// same-context persisted-origins fallback for dynamic registry/config-file
/// origins that vanished with their folder (recorded only on complete scopes,
/// re-admitted only under an equal scope identity).
/// Archive orders are allocated after all live source stages as current
/// per-lane SQL max+1, so carried rows never collide with live orders and
/// registration never evicts a live cohort. Main calls this after live
/// staging, before finalization.
pub(crate) fn carry_absent_durable_sources(
    query: &QueryState,
    cache: &SourceMessageCache,
    context: &crate::ResolvedLocalSourceContext,
    scan: &crate::scanner::ScanResult,
    home: &Path,
    clients: &[String],
) -> Result<(), String> {
    query.finish_seeded_cache_descriptors()?;
    use crate::message_cache::CacheIdentity;
    use std::collections::{HashMap, HashSet};
    let include_all = clients.is_empty();
    let wanted = |namespace: &str| include_all || clients.iter().any(|client| client == namespace);
    // Live scan coverage, by canonical path bytes, so an undiscovered stored
    // source is carried exactly once and a discovered one is never restaged.
    // Both the canonical carry codec and the legacy CachedPath codec suppress
    // a stored twin: the same path must not register twice.
    let mut live: HashSet<Vec<u8>> = HashSet::new();
    let mut cover = |path: &Path| {
        if let Ok(bytes) = crate::message_cache::encode_path_for_carry(path) {
            live.insert(bytes);
        }
        if let Ok(bytes) = bincode::serialize(&crate::message_cache::CachedPath::from_path(path)) {
            live.insert(bytes);
        }
    };
    for paths in scan.files.iter() {
        for path in paths {
            cover(path);
        }
    }
    for path in scan
        .opencode_dbs
        .iter()
        .chain(scan.get(crate::ClientId::OpenCode).iter())
    {
        cover(path);
    }
    for optional in [
        scan.copilot_desktop_db.as_ref(),
        scan.synthetic_db.as_ref(),
        scan.kilo_db.as_ref(),
        scan.hermes_db.as_ref(),
        scan.goose_db.as_ref(),
        scan.zed_db.as_ref(),
        scan.kiro_db.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        cover(optional);
    }
    for source in &scan.crush_dbs {
        cover(&source.db_path);
    }
    if let Some(dir) = &scan.opencode_json_dir {
        cover(dir);
    }
    // Claude identities are home-scoped; an imported scope-0 row stays
    // eligible alongside the captured home scope. Other lanes use the bare
    // namespace. Live precedence is structural: archive orders allocate after
    // all live stages as per-lane max+1, and retained rows replay at phase 2
    // while live rows keep phases 0/1 — so a fresh authoritative same-key
    // fact (including a Claude cache-split rewrite) always wins the fold.
    let claude_scope = CacheIdentity::for_client(crate::ClientId::Claude).with_scope(home);
    let claude_scope = claude_scope.scope_bytes();
    let mut known = cache.stored_sources()?;
    known.sort();
    // Collect first, register after: per-lane archive bases come from current
    // SQL max+1 (one query per lane), so carried orders can never collide
    // with live orders staged earlier in this scope.
    let mut pending: Vec<(String, PathBuf, CachedSourceMetadata)> = Vec::new();
    // Same canonical path under both the captured home scope and an imported
    // scope-0 row registers once; the home-scoped history (with its legacy
    // tail) wins the single slot.
    let mut seen: HashSet<(String, Vec<u8>)> = HashSet::new();
    for (namespace, scope, path) in known {
        if !wanted(&namespace) {
            continue;
        }
        let scoped = namespace == crate::ClientId::Claude.as_str();
        if scoped {
            if scope != claude_scope && scope != [0; 32] {
                continue;
            }
        } else if scope != [0; 32] {
            continue;
        }
        if live.contains(&path) {
            continue;
        }
        let decoded: PathBuf = crate::message_cache::decode_path_for_carry(&path)?;
        if !scan.admits_durable_source(&namespace, &decoded)
            && !same_context_origin_admitted(query, context, &namespace, &decoded)?
        {
            continue;
        }
        let Some(identity) = CacheIdentity::current_for_namespace_carry(&namespace) else {
            continue;
        };
        let identity = if scoped {
            identity.with_scope(home)
        } else {
            identity
        };
        // Absent paths carry last-good; present-but-unreadable paths fail
        // closed (never silently carry stale facts over a permission error).
        match std::fs::symlink_metadata(&decoded) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => continue,
            Err(_) => return Err("durable source discovery failed".into()),
        }
        let history = cache.retention_metadata(identity, &decoded)?;
        let Some(history) = history else { continue };
        // Readability is not warm admission: cacheable=false archived facts
        // still register; live warm helpers keep their own gates.
        let key = (namespace.clone(), path.clone());
        if !seen.insert(key) {
            continue;
        }
        pending.push((namespace.clone(), decoded, history));
    }
    // Home-scoped Claude rows sort before bare scope-0 duplicates of the same
    // path, so the legacy-tailed history wins the single slot; lanes group
    // deterministically for stable archive orders.
    pending.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            let a_home = a.1.starts_with(home);
            let b_home = b.1.starts_with(home);
            b_home.cmp(&a_home).then(a.1.cmp(&b.1))
        })
    });
    let mut grouped: HashMap<String, Vec<(PathBuf, CachedSourceMetadata)>> = HashMap::new();
    for (namespace, decoded, history) in pending {
        grouped
            .entry(namespace)
            .or_default()
            .push((decoded, history));
    }
    let mut lanes: Vec<String> = grouped.keys().cloned().collect();
    lanes.sort();
    if lanes.is_empty() {
        return Ok(());
    }
    let transaction =
        rusqlite::Transaction::new_unchecked(&query.db, rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
    for lane in lanes {
        let cohorts = grouped.remove(&lane).unwrap_or_default();
        if cohorts.is_empty() {
            continue;
        }
        let generation = query.pending_generation()?;
        let base: i64 = query
            .db
            .query_row(
                "SELECT COALESCE(MAX(source_order),-1) FROM sq_scope_sources WHERE scope_generation=?1 AND lane=?2",
                rusqlite::params![generation, lane],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        for (position, (decoded, history)) in cohorts.into_iter().enumerate() {
            let order = (base + 1 + position as i64).max(0) as usize;
            // Carried cohorts replay exactly like live ones, including the
            // raw-source tag the Kiro suppression gate reads.
            let tag = lane == ClientId::Kiro.as_str()
                && sessions::kiro::is_kiro_global_storage_source(&decoded);
            query.observe_cache_on(&transaction, &decoded, &history, &lane, order, tag)?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

impl QueryState {
    pub(crate) fn record_completed_origins(
        &self,
        snapshot: &UsageSnapshot,
        context: &crate::ResolvedLocalSourceContext,
    ) -> Result<(), String> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sq_durable_origins(scope_key BLOB NOT NULL,namespace TEXT NOT NULL,path BLOB NOT NULL,ownership BLOB,PRIMARY KEY(scope_key,namespace,path)) WITHOUT ROWID;
             CREATE UNIQUE INDEX IF NOT EXISTS sq_origin_ownership ON sq_durable_origins(ownership,namespace,path);",
        ).map_err(|error| error.to_string())?;
        // One origin per owning configuration, not per rotating query scope.
        // Claims outlive scope GC, but only a successfully published scope
        // can establish them. Cache rows use the canonical mc path encoding.
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        let mut statement = transaction
            .prepare(
                "SELECT DISTINCT m.namespace,m.path FROM sq_scope_sources s
             JOIN mc_sources m ON m.id=s.cache_source_id
             WHERE s.scope_generation=?1 AND s.kind IN ('cache','observed','observed-legacy')",
            )
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query([snapshot.generation])
            .map_err(|error| error.to_string())?;
        let mut owners = std::collections::HashMap::new();
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let namespace: String = row.get(0).map_err(|error| error.to_string())?;
            let path: Vec<u8> = row.get(1).map_err(|error| error.to_string())?;
            if !owners.contains_key(&namespace) {
                owners.insert(
                    namespace.clone(),
                    source_ownership_digest(context, &namespace)?,
                );
            }
            transaction
                .prepare_cached("INSERT OR IGNORE INTO sq_durable_origins VALUES(?1,?2,?3,?4)")
                .map_err(|error| error.to_string())?
                .execute(params![snapshot.scope, namespace, path, owners[&namespace]])
                .map_err(|error| error.to_string())?;
        }
        drop(rows);
        drop(statement);
        transaction.commit().map_err(|error| error.to_string())
    }
}

fn same_context_origin_admitted(
    query: &QueryState,
    context: &crate::ResolvedLocalSourceContext,
    namespace: &str,
    path: &Path,
) -> Result<bool, String> {
    if !context.admits_scan_root(path) {
        return Ok(false);
    }
    let table: i64 = query
        .db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='sq_durable_origins'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if table == 0 {
        return Ok(false);
    }
    if crate::message_cache::CacheIdentity::current_for_namespace_carry(namespace).is_none() {
        return Ok(false);
    }
    let encoded = crate::message_cache::encode_path_for_carry(path)?;
    let generation = query.pending_generation()?;
    // Claim matches when the staged scope's own key equals the recorded
    // scope_key (exact same scope) OR the current capture's source-ownership
    // digest equals the recorded claim's ownership digest. Ownership covers
    // only what admits a source: home, env-roots mode, resolved client
    // paths, opencode/extra paths, codex archive root, and non-empty
    // exclusions (explicit disable denies). It deliberately excludes the
    // contract version, target-os tag, client registry text, source-env
    // states, platform dirs, cache/routing dirs, capture cwd, and the
    // pricing-cache-only flag — so restarts, upgrades, cwd changes, and
    // pricing-cache flips preserve history while ownership changes deny.
    let ownership = source_ownership_digest(context, namespace)?;
    let admitted: bool = query
        .db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_durable_origins o WHERE o.namespace=?2 AND o.path=?3 AND (o.scope_key=(SELECT s.scope_key FROM sq_scope_generations g JOIN sq_scopes s ON s.id=g.scope_id WHERE g.id=?1) OR o.ownership=?4))",
            rusqlite::params![generation, namespace, encoded, ownership],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(admitted)
}

fn source_ownership_digest(
    context: &crate::ResolvedLocalSourceContext,
    namespace: &str,
) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"durable-origin-ownership-v2");
    hasher.update([u8::from(context.use_env_roots())]);
    hasher.update(context.home_dir().as_os_str().as_encoded_bytes());
    hasher.update([0]);
    let client = crate::ClientId::from_str(namespace);
    if let Some(client) = client {
        let root = context
            .resolve_client_path(client)
            .map_err(|error| error.to_string())?;
        hasher.update(root.as_os_str().as_encoded_bytes());
        hasher.update([0]);
        if client == crate::ClientId::Codex {
            hasher.update(context.codex_archive_root().as_os_str().as_encoded_bytes());
            hasher.update([0]);
        }
    }
    if namespace == "synthetic" || client == Some(crate::ClientId::OpenCode) {
        let root = context
            .resolve_client_root(crate::clients::PathRoot::XdgData)
            .map_err(|error| error.to_string())?;
        hasher.update(root.as_os_str().as_encoded_bytes());
        hasher.update([0]);
    }
    let mut paths: Vec<&Path> = context
        .extra_scan_paths()
        .iter()
        .filter(|(owner, _)| Some(*owner) == client)
        .map(|(_, path)| path.as_path())
        .collect();
    let settings = context.scanner_settings();
    for (owner, roots) in &settings.extra_scan_paths {
        if owner == namespace
            || client.is_some_and(|client| crate::ClientId::from_str(owner) == Some(client))
        {
            paths.extend(roots.iter().map(PathBuf::as_path));
        }
    }
    if client == Some(crate::ClientId::OpenCode) {
        paths.extend(settings.opencode_db_paths.iter().map(PathBuf::as_path));
    }
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        hasher.update([0]);
        hasher.update(path.as_os_str().as_encoded_bytes());
    }
    hasher.update([1]);
    let mut excluded: Vec<&Path> = settings
        .excluded_scan_paths
        .iter()
        .filter(|(owner, _)| {
            owner.as_str() == namespace
                || client.is_some_and(|client| crate::ClientId::from_str(owner) == Some(client))
        })
        .flat_map(|(_, roots)| roots.iter().map(PathBuf::as_path))
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    excluded.sort_unstable();
    excluded.dedup();
    for path in excluded {
        hasher.update([0]);
        hasher.update(path.as_os_str().as_encoded_bytes());
    }
    Ok(hasher.finalize().to_vec())
}

pub(crate) fn cache_hit(
    cache: &SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    fingerprint: Option<&SourceFingerprint>,
    allow_empty: bool,
) -> Result<Option<CachedSourceMetadata>, String> {
    Ok(cache.metadata(identity, path)?.filter(|metadata| {
        fingerprint.is_some_and(|fingerprint| metadata.fingerprint == *fingerprint)
            && (allow_empty || metadata.message_count != 0)
    }))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_cached(
    query: &QueryState,
    _cache: &SourceMessageCache,
    _identity: CacheIdentity,
    path: &Path,
    metadata: &CachedSourceMetadata,
    lane: &str,
    index: usize,
    phase: i32,
    tag: bool,
    headless: bool,
) -> Result<(), String> {
    query.register_cache(path, metadata, lane, index, phase, tag, headless)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_source(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    fingerprint: Option<SourceFingerprint>,
    messages: Vec<UnifiedMessage>,
    lane: &str,
    index: usize,
    phase: i32,
    tag: bool,
    _remove_empty: bool,
) -> Result<(), String> {
    let fingerprint = fingerprint
        .ok_or_else(|| format!("{lane} source fingerprint unavailable; previous scope retained"))?;
    let metadata = cache.write_source(
        identity,
        path,
        fingerprint,
        None,
        None,
        None,
        &mut |_| {},
        &mut |db, metadata| {
            query.register_cache_on(db, path, metadata, lane, index, phase, tag, false)
        },
        |writer| {
            for message in &messages {
                writer.push(message, false, false)?;
            }
            Ok(())
        },
    )?;
    stage_cached(
        query, cache, identity, path, &metadata, lane, index, phase, tag, false,
    )
}
#[cfg(test)]
type PiMessageHook = Box<dyn FnMut(&Path) -> Result<(), String>>;

#[cfg(test)]
thread_local! {
    static PI_MESSAGE_HOOK: std::cell::RefCell<Option<PiMessageHook>> =
        std::cell::RefCell::new(None);
}

/// Pi-family sources share Codex's frozen-prefix storage, but keep their own
/// parser state. Prefix admission hashes every prior byte; it is not an
/// inode/size or sampled-fingerprint append guess.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_pi_source(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    before: Option<SourceFingerprint>,
    index: usize,
    phase: i32,
    tag: bool,
) -> Result<(), String> {
    let fingerprint = before.ok_or_else(|| "Pi source fingerprint unavailable; previous scope retained".to_string())?;
    let cached = cache.metadata(identity, path)?;
    if let Some(metadata) = cached.as_ref().filter(|metadata| metadata.message_count != 0 && metadata.fingerprint == fingerprint) {
        return stage_cached(query, cache, identity, path, metadata, identity.namespace, index, 0, tag, false);
    }
    let append = cached.as_ref().filter(|metadata| metadata.retained_count == 0
        && metadata.fallback_count == 0
        && metadata.pi_incremental.as_ref().is_some_and(|state| state.resumable
            && state.session_id.is_some()
            && state.consumed_offset == metadata.fingerprint.size
            && state.prefix_hash == metadata.fingerprint.content_hash
            && fingerprint.size > state.consumed_offset
            && crate::message_cache::hash_prefix(path, state.consumed_offset) == Some(state.prefix_hash)));
    let previous = append.and_then(|metadata| metadata.pi_incremental.clone());
    let dedup = if identity.namespace == "kimchi" {
        sessions::pi::PiDedup::SessionScoped
    } else {
        sessions::pi::PiDedup::CrossSession
    };
    // Parse inside the canonical write transaction: any read, sink or final
    // full-fingerprint failure rolls back both facts and scope registration.
    // No full-history TEMP serialization/replay is needed.
    cache.write_source(
        identity, path, fingerprint.clone(), None, append, None, &mut |_| {},
        &mut |db, metadata| {
            query.register_cache_on(db, path, metadata, identity.namespace, index, phase, tag, false)
        },
        |writer| {
            let mut state = sessions::utils::source_read_scope(path, || {
                sessions::pi::visit_pi_format_incremental(path, identity.namespace, dedup, previous,
                    &mut |message| {
                        writer.push(&message, false, false)?;
                        #[cfg(test)]
                        PI_MESSAGE_HOOK.with(|slot| {
                            if let Some(hook) = slot.borrow_mut().as_mut() { hook(path) } else { Ok(()) }
                        })?;
                        Ok(())
                    })
            })??;
            if SourceFingerprint::from_path(path).as_ref() != Some(&fingerprint) {
                return Err("Pi source changed during ingestion; previous generation retained".into());
            }
            state.prefix_hash = fingerprint.content_hash;
            writer.set_pi_incremental(state);
            Ok(())
        },
    )?;
    Ok(())
}

pub(crate) fn pi_source_fingerprint(
    cache: &SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
) -> Result<Option<SourceFingerprint>, String> {
    let cached = cache.metadata(identity, path)?.filter(|metadata| metadata.message_count != 0);
    Ok(match SourceFingerprint::check_path(path, cached.as_ref().map(|metadata| &metadata.fingerprint)) {
        Some(crate::message_cache::FingerprintStatus::Unchanged) => cached.map(|metadata| metadata.fingerprint),
        Some(crate::message_cache::FingerprintStatus::Changed(fingerprint)) => Some(fingerprint),
        None => None,
    })
}

pub(crate) fn ingest_pi_sources(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    paths: &[std::path::PathBuf],
    mode: SelectionMode,
) -> Result<(), String> {
    ingest_sources_inner(query, cache, identity, paths, identity.namespace, mode,
        &|cache, path| pi_source_fingerprint(cache, identity, path), &|_| true, false,
        |query, cache, path, before, index, phase, tag| {
            stage_pi_source(query, cache, identity, path, before, index, phase, tag)
        })
}

/// Stream one source into the cache without ever holding its messages in a
/// `Vec`. Rows flow straight from the visitor into a query-scratch lane, the
/// fingerprint is re-verified before the cache write commits, and the scratch
/// lane is released once the generation is durable. Any producer, read-scope,
/// or fingerprint failure returns before `write_source` commits, so the scope
/// publishes nothing partial; the transactional `write_source` rolls back
/// mid-commit producer failures the same way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn stage_streamed_source(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    before: Option<SourceFingerprint>,
    refingerprint: impl Fn(&Path) -> Option<SourceFingerprint>,
    visit: impl FnOnce(&mut dyn FnMut(UnifiedMessage) -> Result<(), String>) -> Result<(), String>,
    lane: &str,
    index: usize,
    phase: i32,
    tag: bool,
) -> Result<(), String> {
    let scratch = format!("{lane}-parse-{index}");
    query
        .db
        .execute(
            "DELETE FROM sq_rows WHERE lane=?1",
            params![scratch.as_str()],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())?;
    // Only TEMP rows are written while parsing. Commit this deferred scratch
    // transaction before the cache writer takes the canonical write lock.
    let streamed = (|| {
        let transaction = query
            .db
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        sessions::utils::source_read_scope(path, || {
            visit(&mut |message| query.push(&scratch, 0, phase, false, false, &message))
        })??;
        transaction.commit().map_err(|error| error.to_string())
    })();
    let result = (|| {
        streamed?;
        if refingerprint(path) != before {
            return Err(format!(
                "{lane} source changed during ingestion; previous generation retained"
            ));
        }
        let fingerprint = before.ok_or_else(|| {
            format!("{lane} source fingerprint unavailable; previous scope retained")
        })?;
        let metadata = cache.write_source(
            identity,
            path,
            fingerprint,
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                query.register_cache_on(db, path, metadata, lane, index, phase, tag, false)
            },
            |writer| {
                query.visit(&scratch, &mut |message, _, _| {
                    writer.push(&message, false, false)
                })
            },
        )?;
        // Ingestion scratch for this source is durable now: release its
        // TEMP rows so later sources in the same scope stage bounded.
        query
            .db
            .execute(
                "DELETE FROM sq_rows WHERE lane=?1",
                params![scratch.as_str()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        stage_cached(
            query, cache, identity, path, &metadata, lane, index, phase, tag, false,
        )
    })();
    if result.is_err() {
        let _ = query.db.execute(
            "DELETE FROM sq_rows WHERE lane=?1",
            params![scratch.as_str()],
        );
    }
    result
}

pub(crate) fn local_dedups(client: ClientId, path: &Path) -> bool {
    matches!(
        client,
        ClientId::Pi
            | ClientId::Kimchi
            | ClientId::Senpi
            | ClientId::Omp
            | ClientId::Junie
            | ClientId::OpenCodeReview
            | ClientId::Augment
            | ClientId::Hindsight
            | ClientId::Muse
            | ClientId::Jcode
            | ClientId::MiMoCode
    ) || (client == ClientId::Kimi && sessions::kimi::is_kimi_code_path(path))
}

pub(crate) fn stage_claude(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    path: &Path,
    home: &Path,
    index: usize,
) -> Result<(), String> {
    let identity = CacheIdentity::for_client(ClientId::Claude).with_scope(home);
    let cached = cache.metadata(identity, path)?;
    let status = SourceFingerprint::check_claude_code_path_with_home_samples_only(
        path,
        cached.as_ref().map(|metadata| &metadata.fingerprint),
        Some(home),
    );
    let fingerprint = match status {
        Some(crate::message_cache::FingerprintStatus::Unchanged) => {
            cached.as_ref().map(|metadata| metadata.fingerprint.clone())
        }
        Some(crate::message_cache::FingerprintStatus::Changed(fingerprint)) => Some(fingerprint),
        None => None,
    };
    if let Some(metadata) =
        cached.filter(|metadata| fingerprint.as_ref() == Some(&metadata.fingerprint))
    {
        return stage_cached(
            query, cache, identity, path, &metadata, "claude", index, 0, false, false,
        );
    }
    let observed = cache.retention_metadata(identity, path)?;
    if let Some(metadata) = observed.as_ref() {
        query.observe_cache(path, metadata, "claude", index, false)?;
    }
    #[cfg(test)]
    crate::RETAIN_OBSERVED_MESSAGES_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let scratch = format!("claude-parse-{index}");
    let mut live_agent = None;
    let mut first = true;
    // Absent transcripts keep durable facts: carry the last-good generation
    // forward as an observed cohort. Present-but-unreadable sources fail
    // closed below via the visitor error.
    if fingerprint.is_none()
        && std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(());
    }
    sessions::claudecode::visit_claude_file_with_home(path, Some(home), &mut |message| {
        if first {
            live_agent = message.agent.clone();
            first = false;
        }
        query.push(&scratch, 0, 0, false, false, &message)
    })?;
    if let Some(fingerprint) = fingerprint {
        let after = SourceFingerprint::check_claude_code_path_with_home_samples_only(
            path,
            None,
            Some(home),
        );
        if !matches!(&after,Some(crate::message_cache::FingerprintStatus::Changed(verified)) if verified==&fingerprint)
        {
            return Err(
                "Claude source changed during ingestion; previous generation retained".into(),
            );
        }
        let metadata = cache.write_source(
            identity,
            path,
            fingerprint,
            observed.as_ref(),
            None,
            None,
            &mut |message| {
                sessions::claudecode::refresh_retained_message_context(
                    message,
                    path,
                    Some(home),
                    live_agent.as_deref(),
                );
            },
            &mut |db, metadata| {
                query.register_cache_on(db, path, metadata, "claude", index, 0, false, false)
            },
            |writer| {
                query.visit(&scratch, &mut |message, _, _| {
                    writer.push(&message, false, false)
                })
            },
        )?;
        // Ingestion scratch for this source is durable now: release its
        // TEMP rows so later sources in the same scope stage bounded.
        query
            .db
            .execute(
                "DELETE FROM sq_rows WHERE lane=?1",
                params![scratch.as_str()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        stage_cached(
            query, cache, identity, path, &metadata, "claude", index, 0, false, false,
        )
    } else {
        Err("Claude source fingerprint unavailable; previous scope retained".into())
    }
}

pub(crate) fn stage_codex(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    path: &Path,
    index: usize,
    headless: bool,
) -> Result<(), String> {
    let identity = CacheIdentity::for_client(ClientId::Codex);
    let cached = cache.metadata(identity, path)?;
    let status =
        SourceFingerprint::check_path(path, cached.as_ref().map(|metadata| &metadata.fingerprint));
    let fingerprint = match status {
        Some(crate::message_cache::FingerprintStatus::Unchanged) => {
            cached.as_ref().map(|metadata| metadata.fingerprint.clone())
        }
        Some(crate::message_cache::FingerprintStatus::Changed(fingerprint)) => Some(fingerprint),
        None => None,
    };
    if let Some(metadata) = cached
        .as_ref()
        .filter(|metadata| fingerprint.as_ref() == Some(&metadata.fingerprint))
    {
        return stage_cached(
            query, cache, identity, path, metadata, "codex", index, 0, false, headless,
        );
    }
    let append = cached.as_ref().filter(|metadata| {
        metadata.codex_incremental.as_ref().is_some_and(|state| {
            fingerprint
                .as_ref()
                .is_some_and(|fingerprint| fingerprint.size > state.consumed_offset)
                && crate::message_cache::codex_prefix_matches(path, state)
        })
    });
    if let Some(metadata) = append {
        query.observe_cache(path, metadata, "codex", index, false)?;
    }
    // Absent rollouts keep durable facts via the observed cohort above (when
    // the append candidate exists) or the last-good generation retained in
    // the store. Present-but-unreadable sources fail closed in the parse.
    if fingerprint.is_none()
        && std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        if append.is_none() {
            if let Some(history) = cache.retention_metadata(identity, path)? {
                query.observe_cache(path, &history, "codex", index, false)?;
            }
        }
        return Ok(());
    }
    let mut scratch = format!("codex-parse-{index}");
    let mut turn_ids = sessions::codex::CodexForkTurnIds::default();
    if let Some(metadata) = append {
        turn_ids.set_history(
            cache.open_codex_turn_reader(metadata.source_id, metadata.generation)?,
            metadata.source_id,
            metadata.generation,
        );
    }
    let parse = |scratch: &str, offset, state, turn_ids: &mut sessions::codex::CodexForkTurnIds| {
        sessions::codex::visit_codex_file_incremental(
            path,
            offset,
            state,
            turn_ids,
            &mut |message: &UnifiedMessage, fallback| {
                query.push(scratch, 0, 0, false, fallback, message)
            },
        )
    };
    let mut outcome = if let Some(metadata) = append {
        let state = metadata
            .codex_incremental
            .as_ref()
            .expect("append candidate has verified state");
        parse(
            &scratch,
            state.consumed_offset,
            state.state.clone(),
            &mut turn_ids,
        )?
    } else {
        parse(
            &scratch,
            0,
            sessions::codex::CodexParseState::default(),
            &mut turn_ids,
        )?
    };
    let mut append = append;
    if append.is_some() && (!outcome.parse_succeeded || outcome.unresolved_model_events) {
        // The full retry parses under a fresh scratch lane; drop the stale
        // incremental attempt's rows so only the retried payload stages.
        let stale = std::mem::take(&mut scratch);
        query
            .db
            .execute("DELETE FROM sq_rows WHERE lane=?1", params![stale.as_str()])
            .map(|_| ())
            .map_err(|error| error.to_string())?;
        scratch = format!("{stale}-full");
        turn_ids.clear()?;
        outcome = parse(
            &scratch,
            0,
            sessions::codex::CodexParseState::default(),
            &mut turn_ids,
        )?;
        append = None;
    }
    if !outcome.parse_succeeded && query.pending_mode.get() != SelectionMode::UnifiedCompatibility {
        return Err("Codex source parse incomplete; previous scope retained".into());
    }
    let after = match fingerprint {
        Some(fingerprint) => {
            if !matches!(
                SourceFingerprint::check_path(path, Some(&fingerprint)),
                Some(crate::message_cache::FingerprintStatus::Unchanged)
            ) {
                return Err(
                    "Codex source changed during ingestion; previous generation retained".into(),
                );
            }
            fingerprint
        }
        None => return Err("Codex source fingerprint unavailable; previous scope retained".into()),
    };
    let cacheable = outcome.parse_succeeded
        && !outcome.unresolved_model_events
        && after.size == outcome.consumed_offset;
    let incremental = if cacheable {
        crate::message_cache::build_codex_incremental_cache_with_prefix_hash(
            path,
            outcome.consumed_offset,
            outcome.state,
            after.content_hash,
        )
    } else {
        None
    };
    let cacheable = cacheable && incremental.is_some();
    let metadata = cache.write_source(
        identity,
        path,
        after,
        None,
        append,
        incremental,
        &mut |_| {},
        &mut |db, metadata| {
            query.register_cache_on(db, path, metadata, "codex", index, 0, false, headless)
        },
        |writer| {
            writer.set_cacheable(cacheable)?;
            query.visit(&scratch, &mut |message, _, fallback| {
                writer.push(&message, false, fallback)
            })?;
            turn_ids.release_history();
            if append.is_none() || turn_ids.was_reset() {
                writer.clear_codex_turn_ids()?;
            }
            turn_ids.visit(&mut |turn| writer.insert_codex_turn_id(turn))?;
            Ok(())
        },
    )?;
    // Ingestion scratch for this source is durable now: release its
    // TEMP rows so later sources in the same scope stage bounded.
    query
        .db
        .execute(
            "DELETE FROM sq_rows WHERE lane=?1",
            params![scratch.as_str()],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())?;
    stage_cached(
        query, cache, identity, path, &metadata, "codex", index, 0, false, headless,
    )
}

/// Absent sources keep their durable facts: carry the last-good generation
/// forward instead of failing the scope. Returns true when the source is gone
/// and the caller should skip staging it.
#[allow(clippy::too_many_arguments)]
fn carry_absent_source(
    query: &QueryState,
    cache: &SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    lane: &str,
    index: usize,
    tag: bool,
    before: &Option<SourceFingerprint>,
) -> Result<bool, String> {
    if before.is_some() {
        return Ok(false);
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(history) = cache.retention_metadata(identity, path)? {
                query.observe_cache(path, &history, lane, index, tag)?;
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Shared lane workflow for materialized and streamed producers: warm-hit
/// partition, absent retention, presence gate, and per-miss cold staging. Only
/// the cold step differs per producer, so both lanes share this loop instead
/// of duplicating it.
#[allow(clippy::too_many_arguments)]
fn ingest_sources_inner<F, T, S>(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    paths: &[std::path::PathBuf],
    lane: &str,
    mode: SelectionMode,
    fingerprint: &F,
    tag: &T,
    allow_empty: bool,
    mut stage_cold: S,
) -> Result<(), String>
where
    F: Fn(&SourceMessageCache, &Path) -> Result<Option<SourceFingerprint>, String>,
    T: Fn(&Path) -> bool,
    S: FnMut(
        &QueryState,
        &mut SourceMessageCache,
        &Path,
        Option<SourceFingerprint>,
        usize,
        i32,
        bool,
    ) -> Result<(), String>,
{
    // Report mode historically gives warm hits first claim, then cold sources
    // in traversal order. Persist the partition instead of an all-misses Vec.
    let hit_first = mode == SelectionMode::Report
        && !matches!(
            lane,
            "gjc"
                | "trae"
                | "antigravity-cli"
                | "zed"
                | "antigravity"
                | "kilo"
                | "goose"
                | "kiro-db"
                | "synthetic"
                | "crush"
        );
    for (index, path) in paths.iter().enumerate() {
        let before = fingerprint(cache, path)?;
        let hit = cache_hit(cache, identity, path, before.as_ref(), allow_empty)?;
        query.classify(lane, index, hit.is_none())?;
        if let Some(metadata) = hit {
            stage_cached(
                query,
                cache,
                identity,
                path,
                &metadata,
                lane,
                index,
                0,
                tag(path),
                false,
            )?;
        } else if !hit_first {
            // Inaccessible (present but unreadable) sources still fail closed
            // below; absent ones keep durable facts via the carried cohort.
            if carry_absent_source(
                query,
                cache,
                identity,
                path,
                lane,
                index,
                tag(path),
                &before,
            )? {
                continue;
            }
            std::fs::File::open(path).map_err(|error| error.to_string())?;
            stage_cold(query, cache, path, before, index, 0, tag(path))?;
        }
    }
    if hit_first {
        for (index, path) in paths.iter().enumerate() {
            if !query.is_miss(lane, index)? {
                continue;
            }
            let before = fingerprint(cache, path)?;
            if carry_absent_source(
                query,
                cache,
                identity,
                path,
                lane,
                index,
                tag(path),
                &before,
            )? {
                continue;
            }
            std::fs::File::open(path).map_err(|error| error.to_string())?;
            stage_cold(query, cache, path, before, index, 1, tag(path))?;
        }
    }
    cache.check()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_sources(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    paths: &[std::path::PathBuf],
    lane: &str,
    mode: SelectionMode,
    fingerprint: impl Fn(&Path) -> Option<SourceFingerprint>,
    parse: impl Fn(&Path) -> Vec<UnifiedMessage>,
    tag: impl Fn(&Path) -> bool,
    allow_empty: bool,
    remove_empty: bool,
) -> Result<(), String> {
    ingest_sources_inner(
        query,
        cache,
        identity,
        paths,
        lane,
        mode,
        &|_, path| Ok(fingerprint(path)),
        &tag,
        allow_empty,
        |query, cache, path, before, index, phase, tag| {
            let messages = sessions::utils::source_read_scope(path, || parse(path))?;
            if fingerprint(path) != before {
                return Err(format!(
                    "{lane} source changed during ingestion; previous generation retained"
                ));
            }
            stage_source(
                query,
                cache,
                identity,
                path,
                before,
                messages,
                lane,
                index,
                phase,
                tag,
                remove_empty,
            )
        },
    )
}

/// Streaming twin of [`ingest_sources`]: the visitor pushes each row without
/// ever holding the whole source in a `Vec`. Partition, retention, presence,
/// and warm-hit order are the shared seam above; only the cold step streams.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ingest_sources_streamed(
    query: &QueryState,
    cache: &mut SourceMessageCache,
    identity: CacheIdentity,
    paths: &[std::path::PathBuf],
    lane: &str,
    mode: SelectionMode,
    fingerprint: impl Fn(&Path) -> Option<SourceFingerprint>,
    visit: impl Fn(&Path, &mut dyn FnMut(UnifiedMessage) -> Result<(), String>) -> Result<(), String>,
    tag: impl Fn(&Path) -> bool,
    allow_empty: bool,
) -> Result<(), String> {
    ingest_sources_inner(
        query,
        cache,
        identity,
        paths,
        lane,
        mode,
        &|_, path| Ok(fingerprint(path)),
        &tag,
        allow_empty,
        |query, cache, path, before, index, phase, tag| {
            stage_streamed_source(
                query,
                cache,
                identity,
                path,
                before,
                &fingerprint,
                |sink| visit(path, sink),
                lane,
                index,
                phase,
                tag,
            )
        },
    )
}

#[cfg(test)]
thread_local! {
    static REPORT_DECODED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn decode_staged_message(row: &rusqlite::Row<'_>) -> Result<UnifiedMessage, String> {
    use sha2::{Digest, Sha256};
    #[cfg(test)]
    REPORT_DECODED_ROWS.with(|count| count.set(count.get() + 1));
    let value = row.get_ref(0).map_err(|error| error.to_string())?;
    let payload = value.as_blob().map_err(|error| error.to_string())?;
    let digest = match row.get_ref(7).map_err(|error| error.to_string())? {
        rusqlite::types::ValueRef::Null => None,
        rusqlite::types::ValueRef::Blob(bytes) => Some(bytes),
        value => {
            return Err(rusqlite::Error::InvalidColumnType(
                7,
                row.as_ref()
                    .column_name(7)
                    .map_err(|error| error.to_string())?
                    .to_string(),
                value.data_type(),
            )
            .to_string())
        }
    };
    if digest.is_some_and(|digest| Sha256::digest(payload).as_slice() != digest) {
        return Err("usage snapshot row checksum mismatch".into());
    }
    let recorded: Option<u32> = row.get(9).map_err(|error| error.to_string())?;
    let mut message = match recorded {
        None => bincode::deserialize(payload).map_err(|error| error.to_string())?,
        Some(version) => crate::message_cache::decode_stored_message_for_query(version, payload)?,
    };
    message.set_timestamp(row.get(3).map_err(|error| error.to_string())?);
    let workspace = row.get_ref(4).map_err(|error| error.to_string())?;
    let label = row.get_ref(5).map_err(|error| error.to_string())?;
    message.set_workspace(
        bincode::deserialize(workspace.as_blob().map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
        bincode::deserialize(label.as_blob().map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
    );
    crate::apply_headless_agent(&mut message, row.get(6).map_err(|error| error.to_string())?);
    message.model_id = row.get(8).map_err(|error| error.to_string())?;
    Ok(message)
}

fn hash_membership_row(
    hash: &mut sha2::Sha256,
    row: &rusqlite::Row<'_>,
    columns: usize,
) -> Result<(), String> {
    for column in 0..columns {
        hash_membership_value(hash, row, column)?;
    }
    Ok(())
}

impl QueryState {
    /// Freeze ownership first, then fingerprint the exact ordered selected
    /// stream without deserializing or pricing historical messages.
    pub(crate) fn prepare_report_fold(&self, snapshot: &UsageSnapshot) -> Result<(), String> {
        use sha2::{Digest, Sha256};
        self.admit_payload_reader()?;
        self.set_client_eligibility(&snapshot.clients)?;
        let reused = self.take_report_reused();
        self.db.execute_batch(
            "CREATE TEMP TABLE sq_fold_rows(position INTEGER PRIMARY KEY,id INTEGER NOT NULL,
                 lane TEXT NOT NULL,simple INTEGER NOT NULL,guarded INTEGER NOT NULL);
             CREATE TEMP TABLE sq_fold_membership(position INTEGER PRIMARY KEY,membership BLOB NOT NULL);",
        ).map_err(|error| error.to_string())?;
        crate::for_each_usage_lane(SelectionMode::Report, &mut |lane, _, simple, guarded| {
            if !reused || !matches!(lane, "claude" | "codex") {
                self.select_lane(lane, SelectionMode::Report, simple)?;
            }
            // INTEGER PRIMARY KEY assigns the next position in selected-rank
            // order, without a second ROW_NUMBER window and its TEMP stream.
            self.db
                .execute(
                    "INSERT INTO sq_fold_rows(id,lane,simple,guarded)
                 SELECT r.id,?1,?2,?3
                 FROM sq_rows r JOIN sq_selected s ON s.id=r.id
                 WHERE r.lane=?1 ORDER BY s.rank",
                    params![lane, simple, guarded],
                )
                .map(|_| ())
                .map_err(|error| error.to_string())
        })?;
        let stride = self.report_checkpoint_stride()?;
        let mut prior_boundaries = std::collections::BTreeSet::<i64>::new();
        let checkpoints_exist: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='report_checkpoints')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if checkpoints_exist {
            let mut statement = self.db.prepare(
                "SELECT DISTINCT row_count FROM report_checkpoints ORDER BY row_count DESC LIMIT 128",
            ).map_err(|error| error.to_string())?;
            let mut rows = statement.query([]).map_err(|error| error.to_string())?;
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                prior_boundaries.insert(row.get(0).map_err(|error| error.to_string())?);
            }
        }
        let mut hash = Sha256::new();
        hash.update(b"syrtis-ordered-report-membership-v1");
        // OpenCode's fallback is query-dependent. Equal producer membership
        // allows its entire first partition to be skipped; changed membership
        // invalidates the sequential suffix, never merges partial costs.
        // These are owned TEMP copies made under the original pin. Never read
        // canonical scope descriptors here: another publisher may already
        // have retired them. Alias-only changes and exact-payload arbitration
        // must participate even if normalized emitted messages are identical.
        for (sql, columns) in [
            ("SELECT source,source_order,kind FROM ocq_scope ORDER BY kind DESC,source_order,source", 3),
            ("SELECT id,source,kind,source_order,ordinal,provider,timestamp,primary_key,message,payload FROM ocq_rows ORDER BY kind DESC,source_order,ordinal,id", 10),
            ("SELECT id,key,timestamp,position FROM ocq_aliases ORDER BY id,key,position", 4),
            ("SELECT model,workspace,workspace_label,session,payload FROM sq_payload_rows WHERE lane='grok' ORDER BY phase,source_order,id", 5),
        ] {
            let mut statement = self.db.prepare(sql).map_err(|error| error.to_string())?;
            let mut rows = statement.query([]).map_err(|error| error.to_string())?;
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                hash_membership_row(&mut hash, row, columns)?;
            }
        }
        self.record_fold_membership(0, &hash)?;
        let mut statement = self
            .db
            .prepare(
                "SELECT f.position,r.payload,r.payload_digest,r.timestamp,r.workspace,
                 r.workspace_label,r.headless,r.model,r.cache_format,f.lane,f.simple,f.guarded
             FROM sq_fold_rows f JOIN sq_payload_rows r ON r.id=f.id ORDER BY f.position",
            )
            .map_err(|error| error.to_string())?;
        let mut rows = statement.query([]).map_err(|error| error.to_string())?;
        let mut last = 0;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let position: i64 = row.get(0).map_err(|error| error.to_string())?;
            let payload = row.get_ref(1).map_err(|error| error.to_string())?;
            let payload = payload.as_blob().map_err(|error| error.to_string())?;
            let digest = Sha256::digest(payload);
            if let rusqlite::types::ValueRef::Blob(stored) =
                row.get_ref(2).map_err(|error| error.to_string())?
            {
                if stored != digest.as_slice() {
                    return Err("usage snapshot row checksum mismatch".into());
                }
            }
            hash.update(digest);
            // Position and derived overrides, but not a source generation:
            // identical prefixes remain reusable after immutable append.
            for column in 3..12 {
                hash_membership_value(&mut hash, row, column)?;
            }
            last = position;
            if position % stride == 0 || prior_boundaries.contains(&position) {
                self.record_fold_membership(position, &hash)?;
            }
        }
        self.record_fold_membership(last, &hash)
    }

    pub(crate) fn report_checkpoint_stride(&self) -> Result<i64, String> {
        self.db
            .query_row(
                "SELECT COALESCE(MAX(position),0) FROM sq_fold_rows",
                [],
                |row| row.get(0),
            )
            .map(crate::usage_store::report_checkpoint_stride)
            .map_err(|error| error.to_string())
    }

    fn record_fold_membership(&self, position: i64, hash: &sha2::Sha256) -> Result<(), String> {
        use sha2::Digest;
        self.db
            .execute(
                "INSERT OR REPLACE INTO sq_fold_membership VALUES(?1,?2)",
                params![position, hash.clone().finalize().as_slice()],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn visit_report_suffix(
        &self,
        after: i64,
        sink: &mut impl FnMut(i64, &str, bool, bool, UnifiedMessage) -> Result<(), String>,
    ) -> Result<(), String> {
        self.admit_payload_reader()?;
        let mut statement = self
            .db
            .prepare(
                "SELECT r.payload,r.retained,r.global_storage,r.timestamp,r.workspace,
                 r.workspace_label,r.headless,r.payload_digest,r.model,r.cache_format,
                 f.position,f.lane,f.simple,f.guarded
             FROM sq_fold_rows f JOIN sq_payload_rows r ON r.id=f.id
             WHERE f.position>?1 ORDER BY f.position",
            )
            .map_err(|error| error.to_string())?;
        let mut rows = statement
            .query([after])
            .map_err(|error| error.to_string())?;
        while let Some(row) = rows.next().map_err(|error| error.to_string())? {
            let message = decode_staged_message(row)?;
            let lane: String = row.get(11).map_err(|error| error.to_string())?;
            sink(
                row.get(10).map_err(|error| error.to_string())?,
                &lane,
                row.get(12).map_err(|error| error.to_string())?,
                row.get(13).map_err(|error| error.to_string())?,
                message,
            )?;
        }
        Ok(())
    }
}

fn hash_membership_value(
    hash: &mut sha2::Sha256,
    row: &rusqlite::Row<'_>,
    column: usize,
) -> Result<(), String> {
    use rusqlite::types::ValueRef;
    use sha2::Digest;
    match row.get_ref(column).map_err(|error| error.to_string())? {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(value) => {
            hash.update([1]);
            hash.update(value.to_le_bytes());
        }
        ValueRef::Real(value) => {
            hash.update([2]);
            hash.update(value.to_bits().to_le_bytes());
        }
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            hash.update([3]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    Ok(())
}

/// Report-only timestamp state; explicit public sessionize collectors remain
/// unchanged. SQLite spills both per-session sorting and the concurrency sweep.
pub(crate) struct DiskTimeFold {
    db: Connection,
    committed: bool,
    error: Option<String>,
}

impl Drop for DiskTimeFold {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.db.execute_batch("ROLLBACK");
        }
    }
}

impl DiskTimeFold {
    pub(crate) fn new() -> Result<Self, String> {
        let db = Connection::open("").map_err(|error| error.to_string())?;
        db.execute_batch("PRAGMA cache_size=-64; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-64;
            CREATE TABLE times(id INTEGER PRIMARY KEY,client TEXT NOT NULL,session TEXT NOT NULL,timestamp INTEGER NOT NULL);
            CREATE INDEX times_session ON times(client,session,timestamp,id);
            CREATE TABLE intervals(client TEXT NOT NULL,session TEXT NOT NULL,start INTEGER NOT NULL,end INTEGER NOT NULL,active INTEGER NOT NULL);
            CREATE INDEX intervals_start ON intervals(start);").map_err(|error|error.to_string())?;
        // Private scratch input only; no canonical table or public callback
        // shares this connection, so one transaction covers feed+intervals.
        db.execute_batch("BEGIN IMMEDIATE")
            .map_err(|error| error.to_string())?;
        Ok(Self {
            db,
            committed: false,
            error: None,
        })
    }

    pub(crate) fn feed(&mut self, message: &UnifiedMessage) {
        if message.timestamp <= 0 || self.error.is_some() {
            return;
        }
        if let Err(error) = self
            .db
            .prepare_cached("INSERT INTO times(client,session,timestamp) VALUES(?1,?2,?3)")
            .and_then(|mut statement| {
                statement
                    .execute(params![
                        message.client,
                        message.session_id,
                        message.timestamp
                    ])
                    .map(|_| ())
            })
        {
            self.error = Some(error.to_string());
        }
    }

    pub(crate) fn finish(
        mut self,
        idle_gap_ms: i64,
    ) -> Result<(crate::TimeMetrics, std::collections::HashMap<String, i64>), String> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        let result: Result<(), String> = (|| {
            let mut groups = self
                .db
                .prepare_cached("SELECT DISTINCT client,session FROM times ORDER BY client,session")
                .map_err(|error| error.to_string())?;
            let mut rows = groups.query([]).map_err(|error| error.to_string())?;
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                let client: String = row.get(0).map_err(|error| error.to_string())?;
                let session: String = row.get(1).map_err(|error| error.to_string())?;
                let mut statement=self.db.prepare_cached("SELECT timestamp FROM times WHERE client=?1 AND session=?2 ORDER BY timestamp,id").map_err(|error|error.to_string())?;
                let mut times = statement
                    .query(params![client, session])
                    .map_err(|error| error.to_string())?;
                let mut start = None;
                let mut end = 0;
                let mut active = 0_i64;
                while let Some(time) = times.next().map_err(|error| error.to_string())? {
                    let timestamp: i64 = time.get(0).map_err(|error| error.to_string())?;
                    if start.is_none() {
                        start = Some(timestamp);
                    } else {
                        let gap = timestamp - end;
                        if gap <= idle_gap_ms {
                            active = active
                                .checked_add(gap)
                                .ok_or("session active duration overflow")?;
                        }
                    }
                    end = timestamp;
                }
                drop(times);
                drop(statement);
                if let Some(start) = start {
                    self.db
                        .prepare_cached("INSERT INTO intervals VALUES(?1,?2,?3,?4,?5)")
                        .map_err(|error| error.to_string())?
                        .execute(params![client, session, start, end, active])
                        .map(|_| ())
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })();
        result?;
        self.db
            .execute_batch("COMMIT")
            .map_err(|error| error.to_string())?;
        self.committed = true;
        let mut metrics = crate::TimeMetrics {
            total_active_time_ms: 0,
            total_wall_time_ms: 0,
            longest_continuous_ms: 0,
            max_concurrent_sessions: 0,
            session_count: 0,
        };
        let mut daily = std::collections::HashMap::<chrono::NaiveDate, i64>::new();
        let mut statement = self
            .db
            .prepare_cached("SELECT start,end,active FROM intervals ORDER BY start,client,session")
            .map_err(|error| error.to_string())?;
        let mut intervals = statement.query([]).map_err(|error| error.to_string())?;
        let mut continuous: Option<(i64, i64)> = None;
        while let Some(row) = intervals.next().map_err(|error| error.to_string())? {
            let start: i64 = row.get(0).map_err(|error| error.to_string())?;
            let end: i64 = row.get(1).map_err(|error| error.to_string())?;
            let active: i64 = row.get(2).map_err(|error| error.to_string())?;
            metrics.total_active_time_ms = metrics
                .total_active_time_ms
                .checked_add(active)
                .ok_or("session active total overflow")?;
            metrics.total_wall_time_ms = metrics
                .total_wall_time_ms
                .checked_add(end - start)
                .ok_or("session wall total overflow")?;
            metrics.session_count = metrics
                .session_count
                .checked_add(1)
                .ok_or("session count overflow")?;
            if active > 0 {
                let window_end = start
                    .checked_add(active)
                    .ok_or("session activity window overflow")?;
                if let Some((merged_start, merged_end)) = continuous {
                    if start
                        <= merged_end
                            .checked_add(idle_gap_ms)
                            .ok_or("session idle boundary overflow")?
                    {
                        continuous = Some((merged_start, merged_end.max(window_end)));
                    } else {
                        metrics.longest_continuous_ms =
                            metrics.longest_continuous_ms.max(merged_end - merged_start);
                        continuous = Some((start, window_end));
                    }
                } else {
                    continuous = Some((start, window_end));
                }
                let interval = crate::SessionInterval {
                    client: String::new(),
                    session_id: String::new(),
                    start_ts: start,
                    end_ts: end,
                    wall_duration_ms: end - start,
                    active_duration_ms: active,
                    message_count: 0,
                    tokens: crate::TokenBreakdown::default(),
                    cost: 0.0,
                };
                let mut overflow = false;
                crate::sessionize::visit_daily_active_time(
                    &interval,
                    &chrono::Local,
                    &mut |date, value| {
                        let entry = daily.entry(date).or_default();
                        if let Some(total) = entry.checked_add(value) {
                            *entry = total;
                        } else {
                            overflow = true;
                        }
                    },
                );
                if overflow {
                    return Err("daily activity total overflow".into());
                }
            }
        }
        if let Some((start, end)) = continuous {
            metrics.longest_continuous_ms = metrics.longest_continuous_ms.max(end - start);
        }
        if self.db.query_row("SELECT EXISTS(SELECT 1 FROM intervals WHERE end=start AND start=9223372036854775807)",[],|row|row.get::<_,bool>(0)).map_err(|error|error.to_string())? {
            return Err("session concurrency endpoint overflow".into());
        }
        let mut statement=self.db.prepare_cached("SELECT timestamp,delta FROM (SELECT start AS timestamp,1 AS delta FROM intervals UNION ALL SELECT CASE WHEN end<=start THEN start+1 ELSE end END AS timestamp,-1 AS delta FROM intervals) ORDER BY timestamp,delta DESC").map_err(|error|error.to_string())?;
        let mut events = statement.query([]).map_err(|error| error.to_string())?;
        let mut current = 0_i64;
        while let Some(row) = events.next().map_err(|error| error.to_string())? {
            let delta: i64 = row.get(1).map_err(|error| error.to_string())?;
            current += delta;
            metrics.max_concurrent_sessions = metrics
                .max_concurrent_sessions
                .max(u32::try_from(current).map_err(|_| "session concurrency count overflow")?);
        }
        Ok((
            metrics,
            daily
                .into_iter()
                .map(|(date, active)| (date.format("%Y-%m-%d").to_string(), active))
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CostSource, ResolvedLocalSourceContext, TokenBreakdown};
    use tempfile::TempDir;

    include!("store_query_index_tests.rs");

    fn query(root: &Path) -> QueryState {
        SourceMessageCache::open_from_root(Some(root))
            .ensure_open()
            .unwrap();
        crate::usage_store::synchronize_opencode_sources(&[], &[], root, &mut |_, _, _, _, _| {
            Ok(())
        })
        .unwrap();
        QueryState::open(root).unwrap()
    }

    fn message(
        client: &str,
        session: &str,
        timestamp: i64,
        key: Option<&str>,
        input: i64,
    ) -> UnifiedMessage {
        let mut message = UnifiedMessage::new(
            client,
            "model",
            "provider",
            session,
            timestamp,
            TokenBreakdown {
                input,
                ..TokenBreakdown::default()
            },
            0.0,
        );
        message.dedup_key = key.map(str::to_string);
        message
    }

    #[test]
    fn staged_message_borrows_digest_and_returns_owned_output() {
        use rusqlite::types::Value;
        use sha2::{Digest, Sha256};
        let db = Connection::open_in_memory().unwrap();
        let original = message("amp", "session", 100, None, 7);
        let payload = bincode::serialize(&original).unwrap();
        let workspace = bincode::serialize(&original.workspace_key).unwrap();
        let label = bincode::serialize(&original.workspace_label).unwrap();
        let mut statement = db
            .prepare("SELECT ?1,0,0,100,?2,?3,0,?4,'model',NULL")
            .unwrap();
        for digest in [Value::Null, Value::Blob(Sha256::digest(&payload).to_vec())] {
            let decoded = {
                let mut rows = statement
                    .query(params![payload, workspace, label, digest])
                    .unwrap();
                decode_staged_message(rows.next().unwrap().unwrap()).unwrap()
            };
            assert_eq!(bincode::serialize(&decoded).unwrap(), payload);
        }
        for (digest, expected) in [
            (
                Value::Blob(vec![0; 32]),
                "usage snapshot row checksum mismatch".to_string(),
            ),
            (
                Value::Text("wrong type".into()),
                rusqlite::Error::InvalidColumnType(7, "?4".into(), rusqlite::types::Type::Text)
                    .to_string(),
            ),
        ] {
            let mut rows = statement
                .query(params![payload, workspace, label, digest])
                .unwrap();
            assert_eq!(
                decode_staged_message(rows.next().unwrap().unwrap()).unwrap_err(),
                expected,
            );
        }
    }

    fn inputs(query: &QueryState, lane: &str, from: Option<i64>, until: Option<i64>) -> Vec<i64> {
        let mut values = Vec::new();
        query
            .visit_selected_lane(
                lane,
                UsageQuery {
                    from_ms: from,
                    until_ms: until,
                    ..UsageQuery::compact()
                },
                &mut |message, _, _| {
                    values.push(message.tokens.input);
                    Ok(())
                },
            )
            .unwrap();
        values
    }

    #[test]
    fn disk_selection_keeps_lane_isolation_and_exact_empty_key_policy() {
        let root = TempDir::new().unwrap();
        let local = query(root.path());
        for (lane, base) in [("pi", 10), ("omp", 20)] {
            local
                .push(
                    lane,
                    0,
                    0,
                    false,
                    true,
                    &message(lane, "one", 1, Some(""), base),
                )
                .unwrap();
            local
                .push(
                    lane,
                    1,
                    0,
                    false,
                    true,
                    &message(lane, "two", 2, Some(""), base + 1),
                )
                .unwrap();
            local
                .select_lane(lane, SelectionMode::LocalClients, true)
                .unwrap();
            assert_eq!(inputs(&local, lane, None, None), vec![base]);
        }
        let report = query(root.path());
        report
            .push(
                "pi",
                0,
                0,
                false,
                true,
                &message("pi", "one", 1, Some(""), 10),
            )
            .unwrap();
        report
            .push(
                "pi",
                1,
                0,
                false,
                true,
                &message("pi", "two", 2, Some(""), 11),
            )
            .unwrap();
        report
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&report, "pi", None, None), vec![10, 11]);
    }

    #[test]
    fn authority_and_live_precedence_are_resolved_before_indexed_range() {
        let root = TempDir::new().unwrap();
        let query = query(root.path());
        query
            .push(
                "claude",
                0,
                2,
                true,
                false,
                &message("claude", "retained", 200, Some("turn"), 10),
            )
            .unwrap();
        query
            .push(
                "claude",
                1,
                0,
                false,
                false,
                &message("claude", "live", 10, Some("turn"), 20),
            )
            .unwrap();
        query
            .select_lane("claude", SelectionMode::LocalClients, false)
            .unwrap();
        assert!(inputs(&query, "claude", Some(100), None).is_empty());
        query
            .push(
                "copilot",
                0,
                0,
                false,
                false,
                &message("copilot", "shared", 10, Some("otel"), 30),
            )
            .unwrap();
        query
            .push(
                "copilot",
                1,
                0,
                false,
                true,
                &message("copilot", "shared", 200, Some("desktop"), 40),
            )
            .unwrap();
        query
            .select_lane("copilot", SelectionMode::LocalClients, true)
            .unwrap();
        assert!(inputs(&query, "copilot", Some(100), None).is_empty());
        query
            .push(
                "trae",
                0,
                0,
                false,
                false,
                &message("trae", "shared", 100, Some("old"), 50),
            )
            .unwrap();
        query
            .push(
                "trae",
                1,
                0,
                false,
                false,
                &message("trae", "shared", 300, Some("new"), 60),
            )
            .unwrap();
        query
            .select_lane("trae", SelectionMode::LocalClients, false)
            .unwrap();
        assert!(inputs(&query, "trae", Some(50), Some(200)).is_empty());
    }

    #[test]
    fn indexed_range_does_not_decode_unrequested_historical_payloads() {
        let root = TempDir::new().unwrap();
        let query = query(root.path());
        query
            .push(
                "pi",
                0,
                0,
                false,
                true,
                &message("pi", "old", 1, Some("old"), 10),
            )
            .unwrap();
        query
            .push(
                "pi",
                1,
                0,
                false,
                true,
                &message("pi", "recent", 200, Some("recent"), 20),
            )
            .unwrap();
        query
            .db
            .execute("UPDATE sq_rows SET payload=X'00' WHERE timestamp=1", [])
            .unwrap();
        query
            .select_lane("pi", SelectionMode::LocalClients, true)
            .unwrap();
        assert_eq!(inputs(&query, "pi", Some(100), None), vec![20]);
        assert!(query
            .visit_selected_lane("pi", UsageQuery::compact(), &mut |_, _, _| Ok(()))
            .is_err());
    }

    #[test]
    fn kiro_workspace_authority_and_non_global_namespace_remain_independent() {
        let root = TempDir::new().unwrap();
        let query = query(root.path());
        for (index, workspace, input) in [(0, "one", 10), (1, "two", 20)] {
            let mut execution = message("kiro", "session", 100, Some("execution:exec"), input);
            execution.set_workspace(Some(workspace.to_string()), Some(workspace.to_string()));
            query
                .push("kiro", index, 0, false, true, &execution)
                .unwrap();
        }
        let mut snapshot = message(
            "kiro",
            "folder/session",
            101,
            Some("snapshot:globalstorage"),
            30,
        );
        snapshot.set_workspace(Some("one".to_string()), Some("one".to_string()));
        query.push("kiro", 2, 0, false, true, &snapshot).unwrap();
        query
            .push(
                "kiro",
                3,
                0,
                false,
                false,
                &message("kiro", "cli", 102, Some("execution:exec"), 40),
            )
            .unwrap();
        query
            .select_lane("kiro", SelectionMode::Report, false)
            .unwrap();
        assert_eq!(inputs(&query, "kiro", None, None), vec![10, 20, 40]);
    }

    #[test]
    fn chronological_output_keeps_traversal_ties_and_float_order() {
        let root = TempDir::new().unwrap();
        let query = query(root.path());
        for (timestamp, cost) in [(2, 1.0), (1, 1.0e16), (1, -1.0e16)] {
            let mut message = message("amp", "session", timestamp, None, 0);
            message.cost = cost;
            message.cost_source = CostSource::ProviderReported;
            query.buffer_output(&message).unwrap();
        }
        let mut costs = Vec::new();
        query
            .visit_output(
                UsageQuery {
                    chronological: true,
                    ..UsageQuery::compact()
                },
                &mut |message| costs.push(message.cost),
            )
            .unwrap();
        assert_eq!(costs, vec![1.0e16, -1.0e16, 1.0]);
        assert_eq!(costs.into_iter().sum::<f64>(), 1.0);
    }

    #[test]
    fn disk_time_fold_matches_existing_session_semantic_fold() {
        let mut disk = DiskTimeFold::new().unwrap();
        let mut legacy = crate::SessionizeAccumulator::new();
        for message in [
            message("amp", "one", 1_700_000_120_000, None, 1),
            message("amp", "one", 1_700_000_000_000, None, 1),
            message("amp", "one", 1_700_000_900_000, None, 1),
            message("pi", "two", 1_700_000_120_000, None, 1),
            message("pi", "two", 1_700_000_180_000, None, 1),
            message("pi", "zero", 1_700_000_120_000, None, 1),
            message("pi", "undated", 0, None, 1),
        ] {
            disk.feed(&message);
            legacy.feed(&message);
        }
        let intervals = legacy.finalize(crate::DEFAULT_IDLE_GAP_MS);
        let expected = crate::compute_time_metrics(&intervals, crate::DEFAULT_IDLE_GAP_MS);
        let (actual, daily) = disk.finish(crate::DEFAULT_IDLE_GAP_MS).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(daily, crate::compute_daily_active_time(&intervals));
    }

    #[test]
    fn disk_time_fold_keeps_session_boundaries_and_reports_overflow() {
        let mut disk = DiskTimeFold::new().unwrap();
        disk.feed(&message("amp", "one", 1_000, None, 1));
        disk.feed(&message(
            "amp",
            "one",
            1_000 + crate::DEFAULT_IDLE_GAP_MS,
            None,
            1,
        ));
        disk.feed(&message(
            "amp",
            "one",
            1_000 + 2 * crate::DEFAULT_IDLE_GAP_MS + 1,
            None,
            1,
        ));
        disk.feed(&message("pi", "two", 1_000, None, 1));
        // One interval per (client, session) group: idle gaps reduce active
        // time but do not split the interval. "one" spans 2*gap+1 wall ms
        // with gap active; "two" is a single instant.
        let (metrics, _) = disk.finish(crate::DEFAULT_IDLE_GAP_MS).unwrap();
        assert_eq!(metrics.session_count, 2);
        assert_eq!(metrics.max_concurrent_sessions, 2);
        assert_eq!(metrics.total_active_time_ms, crate::DEFAULT_IDLE_GAP_MS);
        assert_eq!(
            metrics.total_wall_time_ms,
            2 * crate::DEFAULT_IDLE_GAP_MS + 1
        );

        let mut overflow = DiskTimeFold::new().unwrap();
        for session in ["one", "two"] {
            overflow.feed(&message("amp", session, 1, None, 1));
            overflow.feed(&message("amp", session, i64::MAX, None, 1));
        }
        // Each session accrues i64::MAX - 1 active ms; the total cannot fit.
        assert!(overflow.finish(i64::MAX).is_err());
    }

    #[test]
    fn all_client_filtering_matches_explicit_client_union() {
        let root = TempDir::new().unwrap();
        let scoped = query(root.path());
        scoped
            .push(
                "pi",
                0,
                0,
                false,
                true,
                &message("pi", "one", 10, Some("a"), 1),
            )
            .unwrap();
        let mut synthetic = message("synthetic", "one", 11, Some("b"), 2);
        synthetic.provider_id = "synthetic".to_string();
        scoped
            .push("synthetic", 1, 0, false, true, &synthetic)
            .unwrap();
        scoped.set_client_eligibility(&[]).unwrap();
        scoped
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        scoped
            .select_lane("synthetic", SelectionMode::Report, true)
            .unwrap();
        let all = inputs(&scoped, "pi", None, None)
            .into_iter()
            .chain(inputs(&scoped, "synthetic", None, None))
            .collect::<Vec<_>>();

        let root_explicit = TempDir::new().unwrap();
        let explicit = query(root_explicit.path());
        explicit
            .push(
                "pi",
                0,
                0,
                false,
                true,
                &message("pi", "one", 10, Some("a"), 1),
            )
            .unwrap();
        let mut synthetic = message("synthetic", "one", 11, Some("b"), 2);
        synthetic.provider_id = "synthetic".to_string();
        explicit
            .push("synthetic", 1, 0, false, true, &synthetic)
            .unwrap();
        explicit
            .set_client_eligibility(&["pi".to_string(), "synthetic".to_string()])
            .unwrap();
        explicit
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        explicit
            .select_lane("synthetic", SelectionMode::Report, true)
            .unwrap();
        let listed = inputs(&explicit, "pi", None, None)
            .into_iter()
            .chain(inputs(&explicit, "synthetic", None, None))
            .collect::<Vec<_>>();
        assert_eq!(all, listed);
        assert_eq!(all, vec![1, 2]);
    }

    #[test]
    #[serial_test::serial]
    fn failed_scope_staging_and_reader_pinning_preserve_published_versions() {
        let root = TempDir::new().unwrap();
        let source = root.path().join("usage.json");
        std::fs::write(&source, b"{\"usage\":1}").unwrap();
        let cache = SourceMessageCache::open_from_root(Some(root.path()));
        cache.ensure_open().unwrap();
        let first = query(root.path());
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().to_path_buf()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let mut snapshot = first
            .begin_scope(
                root.path(),
                &context,
                &["amp".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let identity = CacheIdentity::for_client(ClientId::Amp);
        let fingerprint = SourceFingerprint::from_path_samples_only(&source).unwrap();
        let original = message("amp", "session", 100, Some("key"), 1);
        let metadata = cache
            .write_source(
                identity,
                &source,
                fingerprint.clone(),
                None,
                None,
                None,
                &mut |_| {},
                &mut |db, metadata| {
                    first.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
                },
                |writer| writer.push(&original, false, false),
            )
            .unwrap();
        first.publish(&mut snapshot).unwrap();
        let failed = query(root.path());
        let _candidate = failed
            .begin_scope(
                root.path(),
                &context,
                &["amp".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let error = cache
            .write_source(
                identity,
                &source,
                fingerprint,
                None,
                None,
                None,
                &mut |_| {},
                &mut |db, metadata| {
                    failed.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
                },
                |writer| {
                    writer.push(
                        &message("amp", "session", 100, Some("key"), 99),
                        false,
                        false,
                    )?;
                    Err("injected complete-read failure".to_string())
                },
            )
            .unwrap_err();
        assert!(error.contains("failure"));
        assert_eq!(
            cache
                .metadata(identity, &source)
                .unwrap()
                .unwrap()
                .generation,
            metadata.generation
        );
        drop(failed);
        let reader = query(root.path());
        reader.pin(&snapshot).unwrap();
        std::fs::write(&source, b"{\"usage\":2}").unwrap();
        let changed = query(root.path());
        let mut next = changed
            .begin_scope(
                root.path(),
                &context,
                &["amp".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        cache
            .write_source(
                identity,
                &source,
                SourceFingerprint::from_path_samples_only(&source).unwrap(),
                None,
                None,
                None,
                &mut |_| {},
                &mut |db, metadata| {
                    changed.register_cache_on(db, &source, metadata, "amp", 0, 0, false, false)
                },
                |writer| {
                    writer.push(
                        &message("amp", "session", 100, Some("key"), 2),
                        false,
                        false,
                    )
                },
            )
            .unwrap();
        changed.publish(&mut next).unwrap();
        assert_ne!(snapshot.generation(), next.generation());
        reader.replay_snapshot(&snapshot).unwrap();
        reader
            .select_lane("amp", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&reader, "amp", None, None), vec![1]);
        reader.unpin().unwrap();
        assert!(query(root.path()).pin(&snapshot).is_err());
        assert!(!format!("{snapshot:?}").contains(&root.path().to_string_lossy().to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn report_phase_transition_changes_winner_without_rewriting_source_facts() {
        let root = TempDir::new().unwrap();
        let cache = SourceMessageCache::open_from_root(Some(root.path()));
        cache.ensure_open().unwrap();
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().to_path_buf()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let paths = [root.path().join("a.json"), root.path().join("b.json")];
        for path in &paths {
            std::fs::write(path, b"{\"usage\":1}").unwrap();
        }
        let mixed = query(root.path());
        let mut mixed_snapshot = mixed
            .begin_scope(
                root.path(),
                &context,
                &["codebuff".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let identity = CacheIdentity::for_client(ClientId::Codebuff);
        let mut metadata = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            metadata.push(
                cache
                    .write_source(
                        identity,
                        path,
                        SourceFingerprint::from_path_samples_only(path).unwrap(),
                        None,
                        None,
                        None,
                        &mut |_| {},
                        &mut |db, meta| {
                            mixed.register_cache_on(
                                db,
                                path,
                                meta,
                                "codebuff",
                                index,
                                if index == 0 { 1 } else { 0 },
                                false,
                                false,
                            )
                        },
                        |writer| {
                            writer.push(
                                &message(
                                    "codebuff",
                                    "session",
                                    100,
                                    Some("duplicate"),
                                    10 + index as i64,
                                ),
                                false,
                                false,
                            )
                        },
                    )
                    .unwrap(),
            );
        }
        mixed.publish(&mut mixed_snapshot).unwrap();
        let reader = query(root.path());
        reader.pin(&mixed_snapshot).unwrap();
        reader.replay_snapshot(&mixed_snapshot).unwrap();
        reader
            .select_lane("codebuff", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&reader, "codebuff", None, None), vec![11]);
        reader.unpin().unwrap();
        let source_generation: i64 = mixed
            .db
            .query_row(
                "SELECT value FROM mc_meta WHERE key='generation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let warm = query(root.path());
        let mut warm_snapshot = warm
            .begin_scope(
                root.path(),
                &context,
                &["codebuff".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        // Targeted capture must warm unchanged descriptors in every Report
        // lane, not only the producer named in its changed-path plan.
        assert!(warm
            .copy_scope_sources(&mixed_snapshot, &warm_snapshot)
            .unwrap());
        for (index, path) in paths.iter().enumerate() {
            let current = cache.metadata(identity, path).unwrap().unwrap();
            assert_eq!(current.generation, metadata[index].generation);
        }
        warm.publish(&mut warm_snapshot).unwrap();
        assert_ne!(mixed_snapshot.generation(), warm_snapshot.generation());
        assert_eq!(
            source_generation,
            warm.db
                .query_row(
                    "SELECT value FROM mc_meta WHERE key='generation'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap()
        );
        let reader = query(root.path());
        reader.pin(&warm_snapshot).unwrap();
        reader.replay_snapshot(&warm_snapshot).unwrap();
        reader
            .select_lane("codebuff", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&reader, "codebuff", None, None), vec![10]);
        reader.unpin().unwrap();
    }

    #[test]
    fn sequential_report_parsers_keep_source_order_after_earlier_source_changes() {
        let root = TempDir::new().unwrap();
        let mut cache = SourceMessageCache::open_from_root(Some(root.path()));
        cache.ensure_open().unwrap();
        for client in [ClientId::Gjc, ClientId::Trae] {
            let paths = [
                root.path().join(format!("{}-a.json", client.as_str())),
                root.path().join(format!("{}-b.json", client.as_str())),
            ];
            let write = |path: &Path, input: i64| {
                let value = if client == ClientId::Gjc {
                    format!(
                        "{}\n{}\n",
                        serde_json::json!({"type":"session","id":"same"}),
                        serde_json::json!({"type":"message","id":"same-turn","message":{"role":"assistant","model":"gpt-5.4","provider":"openai","timestamp":1000,"usage":{"input":input,"output":1}}})
                    )
                } else {
                    serde_json::json!([{"session_id":"same","usage_time":1,"model_name":"GPT-5.4","extra_info":{"input_token":input,"output_token":1},"dollar_float":0.1}]).to_string()
                };
                std::fs::write(path, value).unwrap();
            };
            let parse = |path: &Path| {
                if client == ClientId::Gjc {
                    sessions::gjc::parse_gjc_file(path)
                } else {
                    sessions::trae::parse_trae_file("trae", path)
                }
            };
            for path in &paths {
                write(path, 10);
            }
            let context = ResolvedLocalSourceContext::capture(
                Some(root.path().to_path_buf()),
                false,
                crate::scanner::ScannerSettings::default(),
            )
            .unwrap();
            let first = query(root.path());
            let mut snapshot = first
                .begin_scope(
                    root.path(),
                    &context,
                    &[client.as_str().to_string()],
                    None,
                    SelectionMode::Report,
                )
                .unwrap();
            ingest_sources(
                &first,
                &mut cache,
                CacheIdentity::for_client(client),
                &paths,
                client.as_str(),
                SelectionMode::Report,
                SourceFingerprint::from_path_samples_only,
                parse,
                |_| false,
                false,
                false,
            )
            .unwrap();
            first.publish(&mut snapshot).unwrap();
            write(&paths[0], 99);
            let changed = query(root.path());
            let mut snapshot = changed
                .begin_scope(
                    root.path(),
                    &context,
                    &[client.as_str().to_string()],
                    None,
                    SelectionMode::Report,
                )
                .unwrap();
            ingest_sources(
                &changed,
                &mut cache,
                CacheIdentity::for_client(client),
                &paths,
                client.as_str(),
                SelectionMode::Report,
                SourceFingerprint::from_path_samples_only,
                parse,
                |_| false,
                false,
                false,
            )
            .unwrap();
            changed.publish(&mut snapshot).unwrap();
            let reader = query(root.path());
            reader.pin(&snapshot).unwrap();
            reader.replay_snapshot(&snapshot).unwrap();
            reader
                .select_lane(client.as_str(), SelectionMode::Report, false)
                .unwrap();
            assert_eq!(inputs(&reader, client.as_str(), None, None), vec![99]);
            reader.unpin().unwrap();
        }
    }

    #[test]
    fn grok_model_fallback_precedes_synthetic_report_eligibility() {
        let root = TempDir::new().unwrap();
        let legacy = root
            .path()
            .join(".grok/sessions/project/session-1/updates.jsonl");
        let unified = root.path().join(".grok/logs/unified.jsonl");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::create_dir_all(unified.parent().unwrap()).unwrap();
        std::fs::write(
            legacy.parent().unwrap().join("signals.json"),
            serde_json::json!({"primaryModelId":"hf:org/Model"}).to_string(),
        )
        .unwrap();
        std::fs::write(&legacy,format!("{}\n",serde_json::json!({"method":"session/update","params":{"sessionId":"session-1","update":{"sessionUpdate":"turn_completed","usage":{"inputTokens":30,"outputTokens":2,"totalTokens":32}},"_meta":{"eventId":"legacy","agentTimestampMs":1785456000000_i64}}}))).unwrap();
        std::fs::write(&unified,format!("{}\n",serde_json::json!({"ts":"2026-07-31T00:00:00Z","pid":23,"sid":"session-1","msg":"shell.turn.inference_done","ctx":{"loop_index":1,"prompt_tokens":10,"completion_tokens":2}}))).unwrap();
        let legacy = sessions::grok::parse_grok_file(&legacy);
        let unified = sessions::grok::parse_grok_file(&unified);
        assert!(!legacy.is_empty());
        assert_eq!(unified[0].model_id, "grok-unknown");
        let raw = legacy.into_iter().chain(unified).collect::<Vec<_>>();
        let expected = sessions::grok::prefer_unified_log_messages(raw.clone())
            .into_iter()
            .filter(|message| {
                sessions::synthetic::matches_synthetic_filter(
                    &message.client,
                    &message.model_id,
                    &message.provider_id,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 1);
        let query = query(root.path());
        for (index, message) in raw.iter().enumerate() {
            query.push("grok", index, 0, false, false, message).unwrap();
        }
        query
            .set_client_eligibility(&["synthetic".to_string()])
            .unwrap();
        query
            .select_lane("grok", SelectionMode::Report, true)
            .unwrap();
        let mut actual = Vec::new();
        query
            .visit_selected_lane("grok", UsageQuery::compact(), &mut |mut message, _, _| {
                query.grok_select(&mut message)?;
                actual.push(message);
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, expected);
    }
    #[test]
    fn carried_archive_orders_allocate_after_live_stages() {
        // Deleted files must not evict live cohorts: archive orders allocate
        // as per-lane max+1 after every live stage, never from zero. Both
        // contributions survive (live 10 + carried 11).
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let sessions = home.join(".pi/agent/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let live_path = sessions.join("live.jsonl");
        let gone_path = sessions.join("gone.jsonl");
        std::fs::write(&live_path, b"{\"usage\":1}").unwrap();
        std::fs::write(&gone_path, b"{\"usage\":1}").unwrap();
        let context = ResolvedLocalSourceContext::capture(
            Some(home.clone()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let scan =
            crate::scanner::scan_all_clients_with_source_context(&context, &["pi".to_string()])
                .unwrap();
        assert!(scan.admits_durable_source("pi", &live_path));
        assert!(scan.admits_durable_source("pi", &gone_path));
        let cache = SourceMessageCache::open_from_root(Some(root.path()));
        cache.ensure_open().unwrap();
        let staged = query(root.path());
        let mut snapshot = staged
            .begin_scope(
                root.path(),
                &context,
                &["pi".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let identity = CacheIdentity::for_client(ClientId::Pi);
        for (index, (path, input, key)) in [(&live_path, 10, "k-live"), (&gone_path, 11, "k-gone")]
            .iter()
            .enumerate()
        {
            let fingerprint = SourceFingerprint::from_path_samples_only(path).unwrap();
            cache
                .write_source(
                    identity,
                    path,
                    fingerprint,
                    None,
                    None,
                    None,
                    &mut |_| {},
                    &mut |db, meta| {
                        staged.register_cache_on(db, path, meta, "pi", index, 0, false, false)
                    },
                    |writer| {
                        writer.push(&message("pi", "s", 100, Some(*key), *input), false, false)
                    },
                )
                .unwrap();
        }
        staged.publish(&mut snapshot).unwrap();
        // Second scope: gone.jsonl deleted from disk, absent from the scan.
        // A scan-time failure guard: if the source is still listed (e.g. the
        // scan errored), skip rather than assert a false loss.
        std::fs::remove_file(&gone_path).unwrap();
        let rescan =
            crate::scanner::scan_all_clients_with_source_context(&context, &["pi".to_string()])
                .unwrap();
        assert!(rescan.discovery_error.is_none());
        assert!(!rescan.get(ClientId::Pi).contains(&gone_path));
        let next = query(root.path());
        let mut revived = next
            .begin_scope(
                root.path(),
                &context,
                &["pi".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let current = cache.metadata(identity, &live_path).unwrap().unwrap();
        next.register_cache(&live_path, &current, "pi", 0, 0, false, false)
            .unwrap();
        carry_absent_durable_sources(&next, &cache, &context, &rescan, &home, &["pi".to_string()])
            .unwrap();
        next.publish(&mut revived).unwrap();
        let orders: Vec<i64> = next
            .db
            .prepare("SELECT source_order FROM sq_scope_sources WHERE scope_generation=?1 AND lane='pi' ORDER BY source_order")
            .unwrap()
            .query_map([revived.generation], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(orders, vec![0, 1]);
        let reader = query(root.path());
        reader.pin(&revived).unwrap();
        reader.replay_snapshot(&revived).unwrap();
        reader
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&reader, "pi", None, None), vec![10, 11]);
        reader.unpin().unwrap();
    }

    #[test]
    fn frozen_format4_rows_decode_by_recorded_version_at_query() {
        // Query rows carrying frozen fmt-4 bytes must decode by the recorded
        // layout (shared exact decoder), not the current bincode shape.
        // Digest verifies first; canonical facts are never rewritten. The
        // frozen shape is spelled inline field-for-field: a change to either
        // layout breaks this test, which is the point.
        #[derive(serde::Serialize)]
        struct FrozenTokens {
            input: i64,
            output: i64,
            cache_read: i64,
            cache_write: i64,
            reasoning: i64,
            cache_write_1h: i64,
        }
        #[derive(serde::Serialize)]
        struct FrozenMessage {
            client: String,
            model_id: String,
            provider_id: String,
            session_id: String,
            workspace_key: Option<String>,
            workspace_label: Option<String>,
            timestamp: i64,
            date: String,
            tokens: FrozenTokens,
            cost: f64,
            cost_source: crate::CostSource,
            duration_ms: Option<i64>,
            message_count: i32,
            agent: Option<String>,
            dedup_key: Option<String>,
            dedup_aliases: Vec<String>,
            is_turn_start: bool,
        }
        let root = TempDir::new().unwrap();
        let live = message("pi", "s", 100, Some("frozen"), 42);
        let frozen = FrozenMessage {
            client: live.client.clone(),
            model_id: live.model_id.clone(),
            provider_id: live.provider_id.clone(),
            session_id: live.session_id.clone(),
            workspace_key: live.workspace_key.clone(),
            workspace_label: live.workspace_label.clone(),
            timestamp: live.timestamp,
            date: live.date.clone(),
            tokens: FrozenTokens {
                input: live.tokens.input,
                output: live.tokens.output,
                cache_read: live.tokens.cache_read,
                cache_write: live.tokens.cache_write,
                reasoning: live.tokens.reasoning,
                cache_write_1h: live.tokens.cache_write_1h,
            },
            cost: live.cost,
            cost_source: live.cost_source,
            duration_ms: live.duration_ms,
            message_count: live.message_count,
            agent: live.agent.clone(),
            dedup_key: live.dedup_key.clone(),
            dedup_aliases: live.dedup_aliases.clone(),
            is_turn_start: live.is_turn_start,
        };
        let bytes = bincode::options()
            .with_fixint_encoding()
            .serialize(&frozen)
            .unwrap();
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&bytes).to_vec();
        let staged = query(root.path());
        staged
            .db
            .execute(
                "INSERT INTO sq_rows(lane,source_order,phase,retained,global_storage,session,workspace,dedup,model,workspace_label,timestamp,payload,payload_digest,client,provider,cache_format) VALUES('pi',0,2,1,0,?1,?2,?3,'model',?4,100,?5,?6,'pi','provider',4)",
                rusqlite::params![
                    live.session_id,
                    bincode::serialize(&live.workspace_key).unwrap(),
                    live.dedup_key,
                    bincode::serialize(&live.workspace_label).unwrap(),
                    bytes,
                    digest,
                ],
            )
            .unwrap();
        staged
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        assert_eq!(inputs(&staged, "pi", None, None), vec![42]);
    }

    #[test]
    #[serial_test::serial]
    fn scope_registry_eviction_is_bounded_and_native_readers_survive() {
        let root = TempDir::new().unwrap();
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().to_path_buf()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let first = query(root.path());
        let mut original = first
            .begin_scope(
                root.path(),
                &context,
                &["original".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        first.publish(&mut original).unwrap();
        let reader = query(root.path());
        let changes = reader.db.total_changes();
        reader.pin(&original).unwrap();
        for index in 0..40 {
            let scope = query(root.path());
            let mut snapshot = scope
                .begin_scope(
                    root.path(),
                    &context,
                    &[format!("scope-{index}")],
                    None,
                    SelectionMode::Report,
                )
                .unwrap();
            scope.publish(&mut snapshot).unwrap();
        }
        assert_eq!(
            first
                .db
                .query_row(
                    "SELECT COUNT(*) FROM sq_scopes WHERE current_generation IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            32
        );
        reader.replay_snapshot(&original).unwrap();
        reader.unpin().unwrap();
        assert_eq!(
            changes,
            reader.db.total_changes(),
            "pure empty read must not mutate canonical data"
        );
        assert!(query(root.path()).pin(&original).is_err());
        assert_eq!(
            first
                .db
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name='sq_scope_readers'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let warm = query(root.path());
        let clients = ["scope-39".to_string()];
        let mut snapshot = warm
            .begin_scope(root.path(), &context, &clients, None, SelectionMode::Report)
            .unwrap();
        let previous = warm
            .db
            .query_row(
                "SELECT current_generation FROM sq_scopes WHERE scope_key=?1",
                [&snapshot.scope],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        warm.publish(&mut snapshot).unwrap();
        assert_eq!(snapshot.generation, previous);
    }

    #[test]
    #[serial_test::serial]
    fn abandoned_candidates_are_bounded_and_evicted_writers_reject() {
        let root = TempDir::new().unwrap();
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().to_path_buf()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let oldest = query(root.path());
        let mut snapshot = oldest
            .begin_scope(
                root.path(),
                &context,
                &["candidate-0".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let mut active = Vec::new();
        for index in 1..40 {
            let candidate = query(root.path());
            candidate
                .begin_scope(
                    root.path(),
                    &context,
                    &[format!("candidate-{index}")],
                    None,
                    SelectionMode::Report,
                )
                .unwrap();
            active.push(candidate);
        }
        assert_eq!(
            oldest
                .db
                .query_row(
                    "SELECT COUNT(*) FROM sq_scope_generations WHERE complete=0",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            32
        );
        assert!(oldest.publish(&mut snapshot).is_err());
        drop(active);
    }

    #[test]
    #[serial_test::serial]
    fn late_source_edits_publish_captured_facts_and_refresh_next_scope() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let variant = home.join(".cc-mirror/one/variant.json");
        let source = home.join(".cc-mirror/one/config/projects/project/session.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(
            &variant,
            serde_json::json!({"name":"one","provider":"anthropic"}).to_string(),
        )
        .unwrap();
        let write_source = |input| {
            std::fs::write(&source,format!("{}\n",serde_json::json!({"type":"assistant","timestamp":"2026-07-31T00:00:00Z","requestId":"request","message":{"id":"turn","model":"claude-sonnet-4","usage":{"input_tokens":input,"output_tokens":2}}}))).unwrap();
        };
        write_source(10);
        let cache_root = root.path().join("cache");
        let mut cache = SourceMessageCache::open_from_root(Some(&cache_root));
        let context = ResolvedLocalSourceContext::capture(
            Some(home.clone()),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let clients = ["claude".to_string(), "gjc".to_string()];
        let staged = query(&cache_root);
        let mut captured = staged
            .begin_scope(&cache_root, &context, &clients, None, SelectionMode::Report)
            .unwrap();
        stage_claude(&staged, &mut cache, &source, &home, 0).unwrap();
        let later = root.path().join("later.jsonl");
        std::fs::write(&later,format!("{}\n{}\n",serde_json::json!({"type":"session","id":"later"}),serde_json::json!({"type":"message","id":"later-turn","message":{"role":"assistant","model":"gpt-5.4","provider":"openai","timestamp":1000,"usage":{"input":10,"output":2}}}))).unwrap();
        ingest_sources(
            &staged,
            &mut cache,
            CacheIdentity::for_client(ClientId::Gjc),
            std::slice::from_ref(&later),
            "gjc",
            SelectionMode::Report,
            SourceFingerprint::from_path_samples_only,
            |path| {
                write_source(30);
                std::fs::write(
                    &variant,
                    serde_json::json!({"name":"changed","provider":"openai"}).to_string(),
                )
                .unwrap();
                sessions::gjc::parse_gjc_file(path)
            },
            |_| false,
            false,
            false,
        )
        .unwrap();
        staged.publish(&mut captured).unwrap();
        let read_inputs = |snapshot: &UsageSnapshot| {
            let reader = query(&cache_root);
            reader.pin(snapshot).unwrap();
            reader.replay_snapshot(snapshot).unwrap();
            reader
                .select_lane("claude", SelectionMode::Report, false)
                .unwrap();
            let values = inputs(&reader, "claude", None, None);
            reader.unpin().unwrap();
            values
        };
        assert_eq!(read_inputs(&captured), vec![10]);

        let next = query(&cache_root);
        let mut refreshed = next
            .begin_scope(&cache_root, &context, &clients, None, SelectionMode::Report)
            .unwrap();
        stage_claude(&next, &mut cache, &source, &home, 0).unwrap();
        next.publish(&mut refreshed).unwrap();
        assert_eq!(read_inputs(&refreshed), vec![30]);

        let invalid = query(&cache_root);
        let mut rejected = invalid
            .begin_scope(&cache_root, &context, &clients, None, SelectionMode::Report)
            .unwrap();
        stage_claude(&invalid, &mut cache, &source, &home, 0).unwrap();
        invalid.db.execute(
            "UPDATE sq_scope_sources SET cache_generation=-1 WHERE scope_generation=?1 AND kind='cache'",
            [rejected.generation],
        ).unwrap();
        assert!(invalid.publish(&mut rejected).is_err());
        assert_eq!(read_inputs(&refreshed), vec![30]);
    }
    #[test]
    fn recreated_store_rejects_a_snapshot_with_reused_scope_generation() {
        let root = TempDir::new().unwrap();
        let cache_root = root.path().join("cache");
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().join("home")),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let first = query(&cache_root);
        let mut old = first
            .begin_scope(
                &cache_root,
                &context,
                &[],
                None,
                SelectionMode::LocalClients,
            )
            .unwrap();
        first.publish(&mut old).unwrap();
        drop(first);
        std::fs::remove_dir_all(&cache_root).unwrap();
        let replacement = query(&cache_root);
        let mut current = replacement
            .begin_scope(
                &cache_root,
                &context,
                &[],
                None,
                SelectionMode::LocalClients,
            )
            .unwrap();
        replacement.publish(&mut current).unwrap();
        assert_eq!(old.generation(), current.generation());
        assert_ne!(old, current);
        assert!(replacement.pin(&old).is_err());
        replacement.pin(&current).unwrap();
        replacement.unpin().unwrap();
    }
    #[test]
    fn pi_cold_refresh_and_direct_producer_rollback() {
        for mode in [SelectionMode::Report, SelectionMode::LocalClients] {
            let root = TempDir::new().unwrap();
            let cache_root = root.path().join("cache");
            let context = ResolvedLocalSourceContext::capture(
                Some(root.path().join("home")), false, crate::scanner::ScannerSettings::default(),
            ).unwrap();
            let content = |id, input| format!("{{\"type\":\"session\",\"id\":\"session\"}}\n{}\n",
                serde_json::json!({"type":"message","id":id,"timestamp":"2026-01-01T00:00:01Z",
                    "message":{"role":"assistant","model":"gpt-5","usage":{"input":input}}}));
            let first = root.path().join("a.jsonl");
            let second = root.path().join("b.jsonl");
            std::fs::write(&first, content("one", 10)).unwrap();
            std::fs::write(&second, content("two", 20)).unwrap();
            let mut cache = SourceMessageCache::open_from_root(Some(&cache_root));
            let identity = CacheIdentity::for_client(ClientId::Omp);
            let staged = query(&cache_root);
            let mut snapshot = staged.begin_scope(&cache_root, &context, &["omp".to_string()],
                None, mode).unwrap();
            let change_first = first.clone();
            let change_second = second.clone();
            let replacement = content("two", 30);
            PI_MESSAGE_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(move |path| {
                if path == change_first { std::fs::write(&change_second, &replacement).unwrap(); }
                Ok(())
            })));
            let result = ingest_pi_sources(&staged, &mut cache, identity,
                &[first.clone(), second], mode);
            PI_MESSAGE_HOOK.with(|slot| slot.borrow_mut().take());
            result.unwrap();
            staged.publish(&mut snapshot).unwrap();
            staged.replay_snapshot(&snapshot).unwrap();
            staged.select_lane("omp", mode, true).unwrap();
            assert_eq!(inputs(&staged, "omp", None, None), vec![10, 30]);
            let original = cache.metadata(identity, &first).unwrap().unwrap();
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).open(&first).unwrap()
                .write_all(content("three", 40).split_once('\n').unwrap().1.as_bytes()).unwrap();
            let rejected = query(&cache_root);
            let _pending = rejected.begin_scope(&cache_root, &context, &["omp".to_string()], None, mode).unwrap();
            let before = pi_source_fingerprint(&cache, identity, &first).unwrap();
            PI_MESSAGE_HOOK.with(|slot| *slot.borrow_mut() =
                Some(Box::new(|_| Err("injected Pi producer abort".into()))));
            let result = stage_pi_source(&rejected, &mut cache, identity, &first, before, 0, 1, true);
            PI_MESSAGE_HOOK.with(|slot| slot.borrow_mut().take());
            assert_eq!(result.unwrap_err(), "injected Pi producer abort");
            let after = cache.metadata(identity, &first).unwrap().unwrap();
            assert_eq!(after.generation, original.generation);
            assert_eq!(after.message_count, 1);
            assert_eq!(after.pi_incremental.unwrap().consumed_offset, original.fingerprint.size);
            let physical_count: u64 = rejected.db.query_row(
                "SELECT count(*) FROM mc_rows WHERE source_id=?1", [original.source_id], |row| row.get(0),
            ).unwrap();
            assert_eq!(physical_count, 1);
        }
    }

    #[test]
    fn pi_family_resume_preserves_frozen_prefix_and_rejects_corruption() {
        use std::io::Write;
        for client in [ClientId::Pi, ClientId::Kimchi, ClientId::Senpi, ClientId::Omp] {
            let root = TempDir::new().unwrap();
            let cache_root = root.path().join("cache");
            let source = root.path().join("session.jsonl");
            let identity = CacheIdentity::for_client(client);
            let lane = client.as_str();
            let context = ResolvedLocalSourceContext::capture(
                Some(root.path().join("home")), false, crate::scanner::ScannerSettings::default(),
            ).unwrap();
            let header = "{\"type\":\"session\",\"id\":\"session\",\"cwd\":\"/tmp\"}\n";
            let record = |id: &str, input: i64| format!("{}\n", serde_json::json!({
                "type":"message","id":id,"timestamp":"2026-01-01T00:00:01Z",
                "message":{"role":"assistant","model":"gpt-5","provider":"openai",
                    "responseId":id,"usage":{"input":input}}
            }));
            std::fs::write(&source, format!("{header}{}", record("one", 10))).unwrap();
            let mut cache = SourceMessageCache::open_from_root(Some(&cache_root));
            let capture = |cache: &mut SourceMessageCache, full: bool| {
                let staged = query(&cache_root);
                let mut snapshot = staged.begin_scope(&cache_root, &context, &[lane.to_string()],
                    None, SelectionMode::Report).unwrap();
                if full {
                    ingest_pi_sources(&staged, cache, identity, std::slice::from_ref(&source),
                        SelectionMode::Report).unwrap();
                } else {
                    let before = pi_source_fingerprint(cache, identity, &source).unwrap();
                    stage_pi_source(&staged, cache, identity, &source, before, 0, 1, true).unwrap();
                }
                staged.publish(&mut snapshot).unwrap();
                (staged, snapshot)
            };
            let (_, first) = capture(&mut cache, true);
            let old_reader = query(&cache_root);
            old_reader.pin(&first).unwrap();
            old_reader.replay_snapshot(&first).unwrap();
            old_reader.select_lane(lane, SelectionMode::Report, true).unwrap();
            assert_eq!(inputs(&old_reader, lane, None, None), vec![10]);
            let first_metadata = cache.metadata(identity, &source).unwrap().unwrap();
            assert!(first_metadata.pi_incremental.is_some());
            let first_physical: i64 = old_reader.db.query_row(
                "SELECT row_generation FROM mc_generation_storage WHERE source_id=?1 AND generation=?2",
                params![first_metadata.source_id,first_metadata.generation], |row| row.get(0),
            ).unwrap();
            // Reopening must recover parser state, not depend on a resident map.
            drop(cache);
            cache = SourceMessageCache::open_from_root(Some(&cache_root));
            let append = |text: &str| {
                std::fs::OpenOptions::new().append(true).open(&source).unwrap()
                    .write_all(text.as_bytes()).unwrap();
            };
            append(&record("two", 20));
            let (second_query, second) = capture(&mut cache, false);
            second_query.replay_snapshot(&second).unwrap();
            second_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            assert_eq!(inputs(&second_query, lane, None, None), vec![10, 20]);
            assert_eq!(inputs(&old_reader, lane, None, None), vec![10]);
            let metadata = cache.metadata(identity, &source).unwrap().unwrap();
            let physical = |generation| second_query.db.query_row(
                "SELECT row_generation FROM mc_generation_storage WHERE source_id=?1 AND generation=?2",
                params![metadata.source_id,generation], |row| row.get::<_, i64>(0),
            ).unwrap();
            assert_eq!(first_physical, physical(metadata.generation));
            assert_eq!(metadata.message_count, 2);
            assert!(metadata.pi_incremental.is_some());
            append(&record("one", 99));
            let (duplicate_query, duplicate) = capture(&mut cache, true);
            duplicate_query.replay_snapshot(&duplicate).unwrap();
            duplicate_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            assert_eq!(inputs(&duplicate_query, lane, None, None), vec![10, 20]);
            let admitted = cache.metadata(identity, &source).unwrap().unwrap();
            let (bytes, digest): (Vec<u8>, Vec<u8>) = duplicate_query.db.query_row(
                "SELECT message,digest FROM mc_logical_rows WHERE source_id=?1 AND generation=?2 AND ordinal=0",
                params![admitted.source_id,admitted.generation], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            let rejected = query(&cache_root);
            let _pending = rejected.begin_scope(&cache_root, &context, &[lane.to_string()], None,
                SelectionMode::Report).unwrap();
            duplicate_query.db.execute("UPDATE mc_rows SET digest=zeroblob(32) WHERE source_id=?1 AND ordinal=0",
                [admitted.source_id]).unwrap();
            append(&record("three", 30));
            let before = pi_source_fingerprint(&cache, identity, &source).unwrap();
            assert!(stage_pi_source(&rejected, &mut cache, identity, &source, before, 0, 1, true).is_err());
            assert_eq!(cache.metadata(identity, &source).unwrap().unwrap().generation, admitted.generation);
            duplicate_query.db.execute("UPDATE mc_rows SET message=?2,digest=?3 WHERE source_id=?1 AND ordinal=0",
                params![admitted.source_id,bytes,digest]).unwrap();
            // Rewriting the old prefix while appending must fall back to a full
            // observation; the stable duplicate identity still resolves first.
            std::fs::write(&source, format!("{header}{}{}", record("one", 11), record("four", 40))).unwrap();
            let (corrected_query, corrected) = capture(&mut cache, false);
            corrected_query.replay_snapshot(&corrected).unwrap();
            corrected_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            let mut corrected_inputs = inputs(&corrected_query, lane, None, None);
            corrected_inputs.sort();
            assert_eq!(corrected_inputs, vec![11, 20, 40]);
            assert!(cache.metadata(identity, &source).unwrap().unwrap().pi_incremental.is_none());
            std::fs::write(&source, header).unwrap();
            let (truncated_query, truncated) = capture(&mut cache, true);
            truncated_query.replay_snapshot(&truncated).unwrap();
            truncated_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            let mut truncated_inputs = inputs(&truncated_query, lane, None, None);
            truncated_inputs.sort();
            assert_eq!(truncated_inputs, vec![11, 20, 40]);
            let tail = record("five", 50);
            let split = tail.len() / 2;
            append(&tail[..split]);
            let (partial_query, partial) = capture(&mut cache, false);
            partial_query.replay_snapshot(&partial).unwrap();
            partial_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            let mut partial_inputs = inputs(&partial_query, lane, None, None);
            partial_inputs.sort();
            assert_eq!(partial_inputs, vec![11, 20, 40]);
            assert!(cache.metadata(identity, &source).unwrap().unwrap().pi_incremental.is_none());
            append(&tail[split..]);
            let (completed_query, completed) = capture(&mut cache, true);
            completed_query.replay_snapshot(&completed).unwrap();
            completed_query.select_lane(lane, SelectionMode::Report, true).unwrap();
            let mut completed_inputs = inputs(&completed_query, lane, None, None);
            completed_inputs.sort();
            assert_eq!(completed_inputs, vec![11, 20, 40, 50]);
            old_reader.unpin().unwrap();
        }
    }

    #[test]
    #[serial_test::serial]
    fn streamed_sink_abort_publishes_nothing_partial() {
        // Pi sink abort: the visitor stops at the first sink Err, the cache
        // write never commits, and the scope keeps no published rows.
        let root = TempDir::new().unwrap();
        let cache_root = root.path().join("cache");
        let mut cache = SourceMessageCache::open_from_root(Some(&cache_root));
        cache.ensure_open().unwrap();
        let context = ResolvedLocalSourceContext::capture(
            Some(root.path().join("home")),
            false,
            crate::scanner::ScannerSettings::default(),
        )
        .unwrap();
        let source = root.path().join("pi.jsonl");
        std::fs::write(
            &source,
            concat!(
                "{\"type\":\"session\",\"id\":\"pi_abort\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"m1\",\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"model\":\"gpt-5.4\",\"provider\":\"openai\",\"usage\":{\"input\":10,\"output\":1}}}\n",
                "{\"type\":\"message\",\"id\":\"m2\",\"timestamp\":\"2026-01-01T00:00:02.000Z\",\"message\":{\"role\":\"assistant\",\"model\":\"gpt-5.4\",\"provider\":\"openai\",\"usage\":{\"input\":20,\"output\":2}}}\n",
            ),
        )
        .unwrap();
        let staged = query(&cache_root);
        let mut snapshot = staged
            .begin_scope(
                &cache_root,
                &context,
                &["pi".to_string()],
                None,
                SelectionMode::Report,
            )
            .unwrap();
        let identity = CacheIdentity::for_client(ClientId::Pi);
        let before = SourceFingerprint::from_path_samples_only(&source);
        let error = stage_streamed_source(
            &staged,
            &mut cache,
            identity,
            &source,
            before,
            SourceFingerprint::from_path_samples_only,
            |sink| {
                sessions::pi::visit_pi_format_file(&source, "pi", sessions::pi::PiDedup::CrossSession, &mut |message| {
                    sink(message)?;
                    Err("injected sink abort".to_string())
                })
            },
            "pi",
            0,
            1,
            false,
        )
        .unwrap_err();
        assert!(error.contains("injected sink abort"));
        assert!(cache.metadata(identity, &source).unwrap().is_none());
        staged.publish(&mut snapshot).unwrap();
        let reader = query(&cache_root);
        reader.pin(&snapshot).unwrap();
        reader.replay_snapshot(&snapshot).unwrap();
        reader
            .select_lane("pi", SelectionMode::Report, true)
            .unwrap();
        assert!(inputs(&reader, "pi", None, None).is_empty());
        reader.unpin().unwrap();
    }
    #[test]
    fn streamed_array_invalid_tail_discards_staged_prefix() {
        for tail in [b",".as_slice(), b",\"\xff\"]".as_slice()] {
            let root = TempDir::new().unwrap();
            let cache_root = root.path().join("cache");
            let mut cache = SourceMessageCache::open_from_root(Some(&cache_root));
            cache.ensure_open().unwrap();
            let context = ResolvedLocalSourceContext::capture(
                Some(root.path().join("home")),
                false,
                crate::scanner::ScannerSettings::default(),
            )
            .unwrap();
            let source = root.path().join("chat-messages.json");
            let mut bytes = br#"[{"variant":"ai","timestamp":"2025-12-20T12:00:05Z","metadata":{"model":"gpt-5","usage":{"inputTokens":10}}}"#.to_vec();
            bytes.extend(std::iter::repeat_n(b' ', 16_384));
            bytes.extend_from_slice(tail);
            std::fs::write(&source, bytes).unwrap();
            let staged = query(&cache_root);
            let mut snapshot = staged
                .begin_scope(
                    &cache_root,
                    &context,
                    &["codebuff".into()],
                    None,
                    SelectionMode::Report,
                )
                .unwrap();
            let emitted = std::cell::Cell::new(0);
            let identity = CacheIdentity::for_client(ClientId::Codebuff);
            let result = ingest_sources_streamed(
                &staged,
                &mut cache,
                identity,
                std::slice::from_ref(&source),
                "codebuff",
                SelectionMode::Report,
                SourceFingerprint::from_path_samples_only,
                |path, sink| {
                    sessions::codebuff::visit_codebuff_file(path, &mut |message| {
                        emitted.set(emitted.get() + 1);
                        sink(message)
                    })
                },
                |_| false,
                false,
            );
            assert!(result.is_err());
            assert_eq!(
                emitted.get(),
                1,
                "exercise rollback after a valid emitted prefix"
            );
            assert!(cache.metadata(identity, &source).unwrap().is_none());
            staged.publish(&mut snapshot).unwrap();
            let reader = query(&cache_root);
            reader.pin(&snapshot).unwrap();
            reader.replay_snapshot(&snapshot).unwrap();
            reader
                .select_lane("codebuff", SelectionMode::Report, false)
                .unwrap();
            assert!(inputs(&reader, "codebuff", None, None).is_empty());
            reader.unpin().unwrap();
        }
    }
}
