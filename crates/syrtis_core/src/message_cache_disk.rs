use super::*;
use rusqlite::{params, Connection, OptionalExtension};
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use std::fmt;
use std::marker::PhantomData;

const SCHEMA_VERSION: i64 = 1;
type StoredSourceIdentity = (String, [u8; 32], Vec<u8>);

#[derive(Debug, Clone)]
pub(crate) struct CachedSourceMetadata {
    pub source_id: i64,
    pub fingerprint: SourceFingerprint,
    pub generation: i64,
    pub cacheable: bool,
    pub message_count: u64,
    pub retained_count: u64,
    pub fallback_count: u64,
    pub codex_incremental: Option<CodexIncrementalCache>,
    /// At most one additional pre-parse legacy snapshot; never message rows.
    pub legacy_history: Option<Box<CachedSourceMetadata>>,
}

struct StoredMetadata {
    id: i64,
    parser_version: u32,
    format_version: u32,
    scope: [u8; 32],
    metadata: CachedSourceMetadata,
    retained_digest: Vec<u8>,
    codex_turn_count: u64,
    codex_turn_digest: Vec<u8>,
}

#[derive(Default)]
struct CacheState {
    connection: Option<Connection>,
    failure: Option<String>,
}

/// Only connection state is resident. Source facts and retained membership stay
/// in the canonical database; an owned entry is an explicit compatibility read.
pub(crate) struct SourceMessageCache {
    cache_root: Option<PathBuf>,
    #[cfg(test)]
    legacy_snapshot: bool,
    state: Mutex<CacheState>,
}

impl Default for SourceMessageCache {
    fn default() -> Self {
        Self::open()
    }
}

fn sql_error(_: rusqlite::Error) -> String {
    "source message cache database operation failed".to_string()
}

fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    bincode::options()
        .serialize(value)
        .map_err(|_| "source message cache serialization failed".to_string())
}

// Compact SQL facts share the query lane's fixed-int codec. Legacy envelopes,
// paths, fingerprints and parser state keep their existing varint codec.
fn encode_fact<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
    bincode::options()
        .with_fixint_encoding()
        .serialize(value)
        .map_err(|_| "source message cache serialization failed".to_string())
}

fn decode_message(bytes: &[u8]) -> Result<UnifiedMessage, String> {
    bincode::options()
        .with_fixint_encoding()
        .with_limit(bytes.len() as u64)
        .deserialize(bytes)
        .map_err(|_| "source message cache row is invalid".to_string())
}

/// Decode one stored fact by the generation's recorded layout. Current rows use
/// the live shape; format-4 rows use the frozen pre-provenance shape whose
/// unknown provenance reads `false`. No blind fallback: an unexpected version
/// is an error, not a guess at another layout.
pub(crate) fn decode_stored_message_for_query(
    format_version: u32,
    bytes: &[u8],
) -> Result<UnifiedMessage, String> {
    decode_stored_message(format_version, bytes)
}

fn decode_stored_message(format_version: u32, bytes: &[u8]) -> Result<UnifiedMessage, String> {
    if format_version == CACHE_FORMAT_VERSION {
        return decode_message(bytes);
    }
    if format_version == 4 {
        let old: format4::UnifiedMessage = bincode::options()
            .with_fixint_encoding()
            .with_limit(bytes.len() as u64)
            .deserialize(bytes)
            .map_err(|_| "source message cache row is invalid".to_string())?;
        return Ok(old.into());
    }
    if format_version == 3 {
        let old: format3::UnifiedMessage = bincode::options()
            .with_fixint_encoding()
            .with_limit(bytes.len() as u64)
            .deserialize(bytes)
            .map_err(|_| "source message cache row is invalid".to_string())?;
        return Ok(old.into());
    }
    Err("source message cache row layout is unsupported".to_string())
}

/// Only rewrite the unpublished copy, never facts held by an older snapshot.
/// Current-layout copies remain SQL-only; migrations decode one fact at a time.
fn migrate_copied_rows(
    connection: &Connection,
    source_id: i64,
    generation: i64,
    format_version: u32,
) -> Result<(), String> {
    if format_version == CACHE_FORMAT_VERSION {
        return Ok(());
    }
    let mut statement = connection.prepare(
        "SELECT ordinal,message,digest FROM mc_rows WHERE source_id=?1 AND generation=?2 ORDER BY ordinal",
    ).map_err(sql_error)?;
    let mut rows = statement
        .query(params![source_id, generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let ordinal: i64 = row.get(0).map_err(sql_error)?;
        let bytes: Vec<u8> = row.get(1).map_err(sql_error)?;
        verify_digest(&bytes, &row.get::<_, Vec<u8>>(2).map_err(sql_error)?)?;
        let bytes = encode_fact(&decode_stored_message(format_version, &bytes)?)?;
        connection.prepare_cached(
            "UPDATE mc_rows SET message=?1,digest=?2 WHERE source_id=?3 AND generation=?4 AND ordinal=?5",
        ).map_err(sql_error)?.execute(params![bytes,Sha256::digest(&bytes).as_slice(),source_id,generation,ordinal])
            .map_err(sql_error)?;
    }
    Ok(())
}

/// Path-codec helpers for durable carry: the scanner and the store must agree
/// on canonical path bytes without re-reading native files.
pub(crate) fn encode_path_for_carry(path: &Path) -> Result<Vec<u8>, String> {
    encode_path(path)
}

pub(crate) fn decode_path_for_carry(bytes: &[u8]) -> Result<std::path::PathBuf, String> {
    decode::<CachedPath>(bytes)
        .map(|path| path.into_path_buf())
        .map_err(|_| "source message cache path decoding failed".to_string())
}

fn encode_path(path: &Path) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        encode(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        let native = CachedPath::from_path(path);
        encode(&native)
    }
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    bincode::options()
        .with_limit(bytes.len() as u64)
        .deserialize(bytes)
        .map_err(|_| "source message cache row is invalid".to_string())
}

fn initialize(connection: &Connection) -> Result<(), String> {
    let existing: u64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('mc_meta','mc_sources','mc_versions','mc_rows','mc_codex_turn_ids','mc_migrations','mc_migration_versions','mc_namespaces')",
        [],|row| row.get(0),
    ).map_err(sql_error)?;
    if existing != 0 && existing != 8 {
        return Err("source message cache schema is incomplete".to_string());
    }
    let transaction = rusqlite::Transaction::new_unchecked(
        connection,
        if existing == 0 {
            rusqlite::TransactionBehavior::Immediate
        } else {
            rusqlite::TransactionBehavior::Deferred
        },
    )
    .map_err(sql_error)?;
    if existing == 0 {
        let admitted: u64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('mc_meta','mc_sources','mc_versions','mc_rows','mc_codex_turn_ids','mc_migrations','mc_migration_versions','mc_namespaces')",
            [], |row| row.get(0),
        ).map_err(sql_error)?;
        if admitted != 0 && admitted != 8 {
            return Err("source message cache schema is incomplete".to_string());
        }
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS mc_meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT OR IGNORE INTO mc_meta VALUES('schema_version',1);
             INSERT OR IGNORE INTO mc_meta VALUES('retained_revision',0);
             INSERT OR IGNORE INTO mc_meta VALUES('recovery_revision',0);
             INSERT OR IGNORE INTO mc_meta VALUES('generation',0);",
            )
            .map_err(sql_error)?;
    }
    let version: i64 = transaction
        .query_row(
            "SELECT value FROM mc_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if version != SCHEMA_VERSION {
        return Err("source message cache schema is unsupported".to_string());
    }
    // Warm admission is read-only: two overlapping readers must never upgrade
    // a stale WAL read snapshot to a writer merely to repeat schema DDL.
    if existing == 0 {
        transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS mc_sources(
            id INTEGER PRIMARY KEY, namespace TEXT NOT NULL, scope BLOB NOT NULL, path BLOB NOT NULL,
            active INTEGER NOT NULL CHECK(active IN (0,1)), generation INTEGER NOT NULL,
            UNIQUE(namespace,scope,path));
         CREATE TABLE IF NOT EXISTS mc_versions(
            source_id INTEGER NOT NULL REFERENCES mc_sources(id) ON DELETE CASCADE,
            generation INTEGER NOT NULL, parser_version INTEGER NOT NULL, format_version INTEGER NOT NULL,
            fingerprint BLOB NOT NULL, resume BLOB, message_count INTEGER NOT NULL,
            retained_count INTEGER NOT NULL, fallback_count INTEGER NOT NULL, retained_digest BLOB NOT NULL,
            cacheable INTEGER NOT NULL CHECK(cacheable IN (0,1)),
            codex_turn_count INTEGER NOT NULL,codex_turn_digest BLOB NOT NULL,
            PRIMARY KEY(source_id,generation)) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS mc_rows(
            source_id INTEGER NOT NULL REFERENCES mc_sources(id) ON DELETE CASCADE,
            generation INTEGER NOT NULL, ordinal INTEGER NOT NULL,
            dedup_key TEXT, retained INTEGER NOT NULL CHECK(retained IN (0,1)),
            fallback INTEGER NOT NULL CHECK(fallback IN (0,1)),
            timestamp INTEGER NOT NULL, message BLOB NOT NULL, digest BLOB NOT NULL,
            session_id TEXT NOT NULL,model_id TEXT NOT NULL,client TEXT NOT NULL,provider_id TEXT NOT NULL,
            workspace BLOB NOT NULL,workspace_label BLOB NOT NULL,
            PRIMARY KEY(source_id,generation,ordinal)) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS mc_rows_key ON mc_rows(source_id,generation,dedup_key,ordinal);
         CREATE INDEX IF NOT EXISTS mc_rows_time ON mc_rows(source_id,generation,timestamp,ordinal);
         CREATE TABLE IF NOT EXISTS mc_codex_turn_ids(
            source_id INTEGER NOT NULL REFERENCES mc_sources(id) ON DELETE CASCADE,
            generation INTEGER NOT NULL,turn_id TEXT NOT NULL,
            PRIMARY KEY(source_id,generation,turn_id)) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS mc_migrations(
            path BLOB PRIMARY KEY, digest BLOB NOT NULL,source_count INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS mc_migration_versions(
            path BLOB NOT NULL REFERENCES mc_migrations(path) ON DELETE CASCADE,
            digest BLOB NOT NULL,source_id INTEGER NOT NULL,generation INTEGER NOT NULL,
            PRIMARY KEY(path,digest,source_id,generation),
            FOREIGN KEY(source_id,generation) REFERENCES mc_versions(source_id,generation)) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS mc_migration_version_pin ON mc_migration_versions(source_id,generation);
         CREATE TABLE IF NOT EXISTS mc_namespaces(namespace TEXT PRIMARY KEY,signature BLOB NOT NULL);",
    ).map_err(sql_error)?;
    }
    // Preparing every required column rejects malformed existing tables before
    // any source can be published. Never repair an incompatible table in place.
    transaction.prepare(
        "SELECT s.id,s.namespace,s.scope,s.path,s.active,s.generation,
                v.parser_version,v.format_version,v.fingerprint,v.resume,v.message_count,
                v.retained_count,v.fallback_count,v.retained_digest,r.ordinal,r.dedup_key,r.retained,r.fallback,r.timestamp,
                v.cacheable,v.codex_turn_count,v.codex_turn_digest,r.message,r.digest,r.session_id,r.model_id,r.client,r.provider_id,r.workspace,r.workspace_label,
                t.turn_id FROM mc_sources s LEFT JOIN mc_versions v ON v.source_id=s.id
                LEFT JOIN mc_rows r ON r.source_id=s.id LEFT JOIN mc_codex_turn_ids t ON t.source_id=s.id LIMIT 0",
    ).map_err(sql_error)?;
    transaction.commit().map_err(sql_error)
}

fn read_metadata(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
) -> Result<Option<StoredMetadata>, String> {
    // Durable queries serve the newest generation of a source identity even
    // when it is unlinked from discovery (active=0): absence only stops
    // future reads from revalidating it, never from serving its facts.
    let current: Option<(i64, i64)> = connection
        .query_row(
            "SELECT id,generation FROM mc_sources WHERE namespace=?1 AND path=?2 AND scope=?3",
            params![
                identity.namespace,
                encode_path(path)?,
                identity.scope.as_slice()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    current
        .map(|(id, generation)| read_version(connection, id, generation))
        .transpose()
}

fn read_version(
    connection: &Connection,
    id: i64,
    generation: i64,
) -> Result<StoredMetadata, String> {
    let (namespace,scope,parser_version,format_version,fingerprint,resume,message_count,retained_count,fallback_count,retained_digest,cacheable,codex_turn_count,codex_turn_digest) = connection.query_row(
        "SELECT s.namespace,s.scope,v.parser_version,v.format_version,v.fingerprint,v.resume,
                v.message_count,v.retained_count,v.fallback_count,v.retained_digest,v.cacheable,v.codex_turn_count,v.codex_turn_digest
         FROM mc_sources s JOIN mc_versions v ON v.source_id=s.id
         WHERE s.id=?1 AND v.generation=?2",
        params![id,generation],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,u32>(2)?,row.get::<_,u32>(3)?,
            row.get::<_,Vec<u8>>(4)?,row.get::<_,Option<Vec<u8>>>(5)?,row.get::<_,u64>(6)?,row.get::<_,u64>(7)?,
            row.get::<_,u64>(8)?,row.get::<_,Vec<u8>>(9)?,row.get::<_,bool>(10)?,row.get::<_,u64>(11)?,row.get::<_,Vec<u8>>(12)?)),
    ).map_err(sql_error)?;
    // Decode known old layouts by their recorded version. Non-retaining
    // namespaces may read the header for a cold reparse, never a warm reuse.
    if format_version != CACHE_FORMAT_VERSION && !matches!(format_version, 3 | 4) {
        return Err("source message cache row layout is unsupported".to_string());
    }
    if generation <= 0
        || retained_count > message_count
        || fallback_count > message_count
        || retained_digest.len() != 32
        || codex_turn_digest.len() != 32
    {
        return Err("source message cache metadata is invalid".to_string());
    }
    let current = CacheIdentity::current_for_namespace(&namespace)
        .ok_or_else(|| "source message cache parser owner is unknown".to_string())?;
    if retained_history_key_filter(&namespace).is_some() && parser_version > current.parser_version
    {
        return Err("source message cache retained parser version is unsupported".to_string());
    }
    let scope: [u8; 32] = scope
        .try_into()
        .map_err(|_| "source message cache context is invalid".to_string())?;
    Ok(StoredMetadata {
        id,
        parser_version,
        format_version,
        scope,
        retained_digest,
        codex_turn_count,
        codex_turn_digest,
        metadata: CachedSourceMetadata {
            source_id: id,
            fingerprint: decode(&fingerprint)?,
            generation,
            cacheable,
            message_count,
            retained_count,
            fallback_count,
            // Stale parser-owned resume state is never decoded against a newer
            // CodexParseState layout; only compact shared rows remain queryable.
            codex_incremental: if parser_version == current.parser_version {
                resume.as_deref().map(decode).transpose()?
            } else {
                None
            },
            legacy_history: None,
        },
    })
}

fn admitted(identity: CacheIdentity, version: u32) -> bool {
    version == identity.parser_version || parser_version_is_migratable(identity.namespace, version)
}

impl SourceMessageCache {
    pub(crate) fn open() -> Self {
        Self {
            cache_root: cache_dir(),
            #[cfg(test)]
            legacy_snapshot: false,
            state: Mutex::new(CacheState::default()),
        }
    }

    pub(crate) fn open_from_root(root: Option<&Path>) -> Self {
        Self {
            cache_root: root.map(Path::to_path_buf),
            #[cfg(test)]
            legacy_snapshot: false,
            state: Mutex::new(CacheState::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn load() -> Self {
        Self::load_with_root(cache_dir())
    }

    #[cfg(test)]
    pub(crate) fn load_from_root(root: Option<&Path>) -> Self {
        Self::load_with_root(root.map(Path::to_path_buf))
    }

    #[cfg(test)]
    fn load_with_root(root: Option<PathBuf>) -> Self {
        let cache = Self {
            cache_root: root,
            legacy_snapshot: true,
            state: Mutex::new(CacheState::default()),
        };
        if let Err(error) = cache.ensure_open() {
            cache.record_failure(error);
        }
        cache
    }

    pub(crate) fn root(&self) -> Result<&Path, String> {
        self.cache_root
            .as_deref()
            .ok_or_else(|| "source message cache root is unavailable".to_string())
    }

    fn with_database<T>(
        &self,
        identity: Option<CacheIdentity>,
        body: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "source message cache connection is unavailable".to_string())?;
        if let Some(error) = state.failure.as_ref() {
            return Err(error.clone());
        }
        if state.connection.is_none() {
            let result: Result<Connection, String> = (|| {
                let connection = crate::usage_store::open_database(self.root()?)?;
                initialize(&connection)?;
                #[cfg(test)]
                connection.execute_batch(
                    "CREATE TEMP TABLE mc_observed(source_id INTEGER PRIMARY KEY,generation INTEGER NOT NULL);",
                ).map_err(sql_error)?;
                #[cfg(test)]
                if self.legacy_snapshot {
                    connection.execute(
                        "INSERT INTO mc_observed SELECT id,generation FROM mc_sources WHERE active=1",[],
                    ).map_err(sql_error)?;
                }
                Ok(connection)
            })();
            match result {
                Ok(connection) => state.connection = Some(connection),
                Err(error) => {
                    state.failure = Some(error.clone());
                    return Err(error);
                }
            }
        }
        let connection = state
            .connection
            .as_ref()
            .expect("initialized cache connection");
        let version: i64 = connection
            .query_row(
                "SELECT value FROM mc_meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if version != SCHEMA_VERSION {
            return Err("source message cache schema is unsupported".to_string());
        }
        if let Some(identity) = identity {
            migrate_namespace(connection, self.root()?, identity)?;
        }
        body(connection)
    }

    pub(crate) fn ensure_open(&self) -> Result<(), String> {
        self.with_database(None, |_| Ok(()))
    }

    #[cfg(test)]
    fn record_failure(&self, error: String) {
        warn_cache_failure_once(
            "source message cache is unavailable",
            self.cache_root.as_deref().unwrap_or(Path::new("")),
            &error,
        );
        if let Ok(mut state) = self.state.lock() {
            state.failure.get_or_insert(error);
        }
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "source message cache connection is unavailable".to_string())?;
        state.failure.clone().map_or(Ok(()), Err)
    }

    pub(crate) fn metadata(
        &self,
        identity: CacheIdentity,
        path: &Path,
    ) -> Result<Option<CachedSourceMetadata>, String> {
        self.with_database(Some(identity), |connection| {
            if legacy_history_exists(connection, identity, path)? {
                return Ok(None);
            }
            Ok(read_metadata(connection, identity, path)?
                .filter(|stored| {
                    stored.format_version == CACHE_FORMAT_VERSION
                        && stored.parser_version == identity.parser_version
                        && stored.metadata.cacheable
                })
                .map(|stored| stored.metadata))
        })
    }

    /// Every stored source identity (namespace, scope, encoded path), active
    /// or unlinked. Lets a fresh scope carry durable-known sources the live
    /// scanner no longer discovers, without re-reading native files.
    pub(crate) fn stored_sources(&self) -> Result<Vec<StoredSourceIdentity>, String> {
        self.with_database(None, |connection| {
            let mut statement = connection
                .prepare("SELECT namespace,scope,path FROM mc_sources ORDER BY namespace,path")
                .map_err(sql_error)?;
            let mut rows = statement.query([]).map_err(sql_error)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().map_err(sql_error)? {
                let namespace: String = row.get(0).map_err(sql_error)?;
                let scope: Vec<u8> = row.get(1).map_err(sql_error)?;
                let scope: [u8; 32] = scope
                    .try_into()
                    .map_err(|_| "source message cache context is invalid".to_string())?;
                let path: Vec<u8> = row.get(2).map_err(sql_error)?;
                out.push((namespace, scope, path));
            }
            Ok(out)
        })
    }

    pub(crate) fn retention_metadata(
        &self,
        identity: CacheIdentity,
        path: &Path,
    ) -> Result<Option<CachedSourceMetadata>, String> {
        self.with_database(Some(identity), |connection| {
            // Cacheability and parser versions gate warm reuse, not durable
            // facts. read_version admits only layouts with a supported codec.
            let history =
                latest_durable_history(connection, identity, path)?.map(|history| history.metadata);
            let legacy = if identity.scope != [0; 32] {
                latest_durable_history(
                    connection,
                    CacheIdentity {
                        scope: [0; 32],
                        ..identity
                    },
                    path,
                )?
                .map(|history| history.metadata)
            } else {
                None
            };
            match history {
                Some(mut history) => {
                    history.legacy_history = legacy.map(Box::new);
                    Ok(Some(history))
                }
                None => Ok(legacy),
            }
        })
    }

    #[cfg(test)]
    /// Rows are decoded singly, including an oversized compact row, which is
    /// never dropped. A pinned version stays queryable after a newer ingestion.
    pub(crate) fn visit_rows<S>(
        &self,
        identity: CacheIdentity,
        path: &Path,
        expected_generation: Option<i64>,
        sink: &mut S,
    ) -> Result<(), String>
    where
        S: FnMut(&UnifiedMessage, bool, bool) -> Result<(), String> + ?Sized,
    {
        self.with_database(Some(identity), |connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            let stored = if let Some(generation) = expected_generation {
                let id: i64 = transaction
                    .query_row(
                        "SELECT id FROM mc_sources WHERE namespace=?1 AND scope=?2 AND path=?3",
                        params![
                            identity.namespace,
                            identity.scope.as_slice(),
                            encode_path(path)?
                        ],
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                read_version(&transaction, id, generation)?
            } else {
                if legacy_history_exists(&transaction, identity, path)? {
                    return Err(
                        "source message cache legacy history requires context refresh".to_string(),
                    );
                }
                read_metadata(&transaction, identity, path)?
                    .filter(|stored| {
                        stored.parser_version == identity.parser_version
                            && stored.metadata.cacheable
                    })
                    .ok_or_else(|| "source message cache source is unavailable".to_string())?
            };
            visit_stored_rows(&transaction, &stored, sink)?;
            transaction.commit().map_err(sql_error)
        })
    }

    #[cfg(test)]
    pub(crate) fn visit_version<S>(
        &self,
        source_id: i64,
        generation: i64,
        sink: &mut S,
    ) -> Result<(), String>
    where
        S: FnMut(&UnifiedMessage, bool, bool) -> Result<(), String> + ?Sized,
    {
        self.with_database(None, |connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            let stored = read_version(&transaction, source_id, generation)?;
            visit_stored_rows(&transaction, &stored, sink)?;
            transaction.commit().map_err(sql_error)
        })
    }

    pub(crate) fn visit_codex_turn_ids<S>(
        &self,
        source_id: i64,
        generation: i64,
        sink: &mut S,
    ) -> Result<(), String>
    where
        S: FnMut(&str) -> Result<(), String> + ?Sized,
    {
        self.with_database(None, |connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            let stored = read_version(&transaction, source_id, generation)?;
            visit_codex_ids(&transaction, &stored, sink)?;
            transaction.commit().map_err(sql_error)
        })
    }

    #[cfg(test)]
    pub(crate) fn visit_version_range<S>(
        &self,
        source_id: i64,
        generation: i64,
        from_ms: Option<i64>,
        until_ms: Option<i64>,
        sink: &mut S,
    ) -> Result<(), String>
    where
        S: FnMut(&UnifiedMessage, bool, bool) -> Result<(), String> + ?Sized,
    {
        self.with_database(None,|connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            let stored = read_version(&transaction,source_id,generation)?;
            validate_stored_counts(&transaction,&stored)?;
            if until_ms == Some(i64::MIN) {return transaction.commit().map_err(sql_error);}
            let upper = until_ms.map_or(i64::MAX,|until| until-1);
            let mut statement = transaction.prepare(
                "SELECT retained,fallback,timestamp,message,digest FROM mc_rows
                 WHERE source_id=?1 AND generation=?2 AND timestamp>=?3 AND timestamp<=?4 ORDER BY timestamp,ordinal",
            ).map_err(sql_error)?;
            let mut rows = statement.query(params![source_id,generation,from_ms.unwrap_or(i64::MIN),upper]).map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let retained: bool = row.get(0).map_err(sql_error)?;
                let fallback: bool = row.get(1).map_err(sql_error)?;
                let timestamp: i64 = row.get(2).map_err(sql_error)?;
                let bytes: Vec<u8> = row.get(3).map_err(sql_error)?;
                verify_digest(&bytes,&row.get::<_,Vec<u8>>(4).map_err(sql_error)?)?;
                let mut message = decode_stored_message(stored.format_version, &bytes)?;
                if fallback {message.set_timestamp(timestamp);}
                sink(&message,retained,fallback)?;
            }
            drop(rows);drop(statement);
            transaction.commit().map_err(sql_error)
        })
    }

    #[cfg(test)]
    pub(crate) fn collect_unreferenced_versions(&self) -> Result<(), String> {
        self.with_database(None, |connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            collect_unreferenced_versions_on(&transaction)?;
            transaction.commit().map_err(sql_error)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_source<F>(
        &self,
        identity: CacheIdentity,
        path: &Path,
        fingerprint: SourceFingerprint,
        observed_from: Option<&CachedSourceMetadata>,
        append_from: Option<&CachedSourceMetadata>,
        codex_incremental: Option<CodexIncrementalCache>,
        refresh_retained: &mut dyn FnMut(&mut UnifiedMessage),
        publish: &mut dyn FnMut(&Connection, &CachedSourceMetadata) -> Result<(), String>,
        producer: F,
    ) -> Result<CachedSourceMetadata, String>
    where
        F: FnOnce(&mut SourceRowWriter<'_>) -> Result<(), String>,
    {
        self.with_database(Some(identity),|connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            // Take the SQLite write lock before reading the previous generation;
            // another writer's committed retained history is part of this merge.
            transaction.execute("UPDATE mc_meta SET value=value WHERE key='schema_version'",[]).map_err(sql_error)?;
            let old = read_metadata(&transaction,identity,path)?;
            let legacy = if identity.scope != [0;32] && retained_history_key_filter(identity.namespace).is_some() {
                read_metadata(&transaction,CacheIdentity { scope: [0;32],..identity },path)?
            } else { None };
            if let Some(old) = old.as_ref() {validate_stored_generation(&transaction,old)?;}
            if let Some(legacy) = legacy.as_ref() {validate_stored_generation(&transaction,legacy)?;}
            let observed = observed_from.map(|expected| read_observed_history(&transaction,identity,path,expected)).transpose()?;
            let observed_legacy = observed_from.and_then(|expected| expected.legacy_history.as_deref())
                .map(|expected| read_observed_history(&transaction,identity,path,expected)).transpose()?;
            let (id,generation) = allocate_generation(&transaction,identity,path,identity.parser_version,&fingerprint,old.as_ref())?;
            let mut writer = SourceRowWriter {
                connection: &transaction,source_id: id,generation,next_ordinal: 0,
                codex_incremental,
                cacheable: true,
                fallback_timestamp: (fingerprint.modified_ns / 1_000_000) as i64,
            };
            if let Some(expected) = append_from {
                let old = old.as_ref().filter(|old| old.parser_version == identity.parser_version && old.metadata.cacheable
                    && old.id == expected.source_id && old.metadata.generation == expected.generation && old.metadata.fingerprint == expected.fingerprint)
                    .ok_or_else(|| "source message cache append generation changed".to_string())?;
                transaction.execute(
                    "INSERT INTO mc_rows SELECT source_id,?1,ordinal,dedup_key,retained,fallback,
                         CASE WHEN fallback=1 THEN ?4 ELSE timestamp END,message,digest,session_id,model_id,client,provider_id,workspace,workspace_label
                     FROM mc_rows WHERE source_id=?2 AND generation=?3",
                    params![generation,id,old.metadata.generation,writer.fallback_timestamp],
                ).map_err(sql_error)?;
                migrate_copied_rows(&transaction,id,generation,old.format_version)?;
                transaction.execute(
                    "INSERT INTO mc_codex_turn_ids SELECT source_id,?1,turn_id FROM mc_codex_turn_ids WHERE source_id=?2 AND generation=?3",
                    params![generation,id,old.metadata.generation],
                ).map_err(sql_error)?;
                writer.next_ordinal = old.metadata.message_count;
            }
            if let Some(mut resume) = writer.codex_incremental.take() {
                for turn_id in resume.state.forked_child_task_started_turn_ids.drain() {
                    writer.insert_codex_turn_id(&turn_id)?;
                }
                writer.codex_incremental = Some(resume);
            }
            producer(&mut writer)?;
            // Durable carry runs for every namespace: Claude reconciles
            // same-key observations unit-aware; other lanes carry missing
            // keyed rows verbatim; keyless rows carry by source-local
            // multiplicity. Fresh producer rows always win their positions.
            let history = latest_durable_history(&transaction,identity,path)?;
            let validated = [old.as_ref(),legacy.as_ref(),observed.as_ref(),observed_legacy.as_ref()];
            if let Some(history) = history.as_ref() {
                if !validated.iter().flatten().any(|known| known.id == history.id
                    && known.metadata.generation == history.metadata.generation) {
                    validate_stored_generation(&transaction,history)?;
                }
            }
            let histories = [observed.as_ref(),observed_legacy.as_ref(),history.as_ref(),legacy.as_ref()];
            let produced_rows = writer.next_ordinal;
            for (index, history) in histories.iter().enumerate() {
                let Some(history) = *history else {continue;};
                if histories[..index].iter().flatten().any(|prior|
                    prior.id == history.id && prior.metadata.generation == history.metadata.generation) {
                    continue;
                }
                merge_history(&mut writer,identity,&fingerprint,produced_rows,Some(history),refresh_retained)?;
            }
            // Fingerprint the exact primary and sibling set again before publish.
            // A failed/incomplete source read cannot remove the last good rows.
            if !fingerprint_is_current(path,&fingerprint) {
                return Err("source message cache source changed during ingestion".to_string());
            }
            // Changed-source queries and subsequent warm reads share one
            // reconciled generation; never publish the pre-merge observation.
            let metadata = publish_generation(&transaction,identity,id,generation,&fingerprint,
                writer.codex_incremental.as_ref(),writer.cacheable,true,old.as_ref())?;
            let query_generation = read_version(&transaction,id,generation)?;
            validate_stored_generation(&transaction,&query_generation)?;
            if writer.cacheable {
                if let Some(legacy) = legacy.as_ref() {delete_source(&transaction,legacy)?;}
            }
            publish(&transaction,&metadata)?;
            transaction.commit().map_err(sql_error)?;
            flush_recovery(connection,self.root()?)?;
            Ok(metadata)
        })
    }

    #[cfg(test)]
    /// Explicit materialization only; never called by report/recent hot paths.
    pub(crate) fn get(&self, identity: CacheIdentity, path: &Path) -> Option<CachedSourceEntry> {
        let result = self.with_database(Some(identity), |connection| {
            materialize(connection, identity, path, true)
        });
        match result {
            Ok(entry) => entry,
            Err(error) => {
                self.record_failure(error);
                None
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn retainable_history(
        &self,
        identity: CacheIdentity,
        path: &Path,
    ) -> Vec<UnifiedMessage> {
        let result = self.with_database(Some(identity), |connection| {
            materialize(connection, identity, path, false)
        });
        match result {
            Ok(entry) => entry.map_or_else(Vec::new, |entry| entry.messages),
            Err(error) => {
                self.record_failure(error);
                Vec::new()
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn insert(&mut self, entry: CachedSourceEntry) {
        let Some(mut identity) = CacheIdentity::current_for_namespace(&entry.parser_namespace)
        else {
            self.record_failure("source message cache parser owner is unknown".to_string());
            return;
        };
        identity.scope = entry.scope;
        if entry.parser_version != identity.parser_version {
            self.record_failure("source message cache parser version is unsupported".to_string());
            return;
        }
        let valid_codex = identity.namespace != ClientId::Codex.as_str()
            || codex_cache_entry_matches_fingerprint(&entry, &entry.fingerprint);
        let path = entry.path.into_path_buf();
        let observed = match self.retention_metadata(identity, &path) {
            Ok(observed) => observed,
            Err(error) => {
                self.record_failure(error);
                return;
            }
        };
        let cacheable = valid_codex
            && (!entry.messages.is_empty()
                || retained_history_key_filter(identity.namespace).is_some()
                    && observed
                        .as_ref()
                        .is_some_and(|history| history.message_count > 0));
        let result = self.write_source(identity,&path,entry.fingerprint,observed.as_ref(),None,entry.codex_incremental,
            &mut |message| crate::sessions::claudecode::refresh_retained_message_context(message,&path,None,None),
            &mut |connection,metadata| {
                connection.execute(
                    "INSERT OR REPLACE INTO mc_observed SELECT id,generation FROM mc_sources WHERE id=?1",
                    [metadata.source_id],
                ).map_err(sql_error)?;Ok(())
            },
            |writer| {
                writer.set_cacheable(cacheable)?;
                for (index,message) in entry.messages.iter().enumerate() {
                    let retained = message.dedup_key.as_ref().is_some_and(|key| entry.retained_keys.contains(key));
                    writer.push(message,retained,entry.fallback_timestamp_indices.contains(&index))?;
                }
                Ok(())
            });
        if let Err(error) = result {
            self.record_failure(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, identity: CacheIdentity, path: &Path) {
        let result = self.with_database(Some(identity), |connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            transaction
                .execute(
                    "UPDATE mc_meta SET value=value WHERE key='schema_version'",
                    [],
                )
                .map_err(sql_error)?;
            if let Some(stored) = read_metadata(&transaction, identity, path)? {
                let observed: Option<i64> = transaction
                    .query_row(
                        "SELECT generation FROM mc_observed WHERE source_id=?1",
                        [stored.id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?;
                // A later committed semantic generation, even under the same
                // raw fingerprint, must survive an earlier parse's invalidation.
                // Explicit invalidation unlinks discovery, never durable
                // facts: the generation stays queryable as canonical history.
                if observed.is_none_or(|expected| expected == stored.metadata.generation) {
                    deactivate_source(&transaction, &stored)?;
                }
                transaction
                    .execute("DELETE FROM mc_observed WHERE source_id=?1", [stored.id])
                    .map_err(sql_error)?;
            }
            transaction.commit().map_err(sql_error)?;
            flush_recovery(connection, self.root()?)
        });
        if let Err(error) = result {
            self.record_failure(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn prune_missing_files(&mut self) {
        let result = self.with_database(None, |connection| {
            for identity in CacheIdentity::all() {
                migrate_namespace(connection, self.root()?, identity)?;
            }
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            transaction
                .execute(
                    "UPDATE mc_meta SET value=value WHERE key='schema_version'",
                    [],
                )
                .map_err(sql_error)?;
            let mut statement = transaction
                .prepare("SELECT namespace,path,scope FROM mc_sources ORDER BY id")
                .map_err(sql_error)?;
            let mut rows = statement.query([]).map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let namespace: String = row.get(0).map_err(sql_error)?;
                let path: Vec<u8> = row.get(1).map_err(sql_error)?;
                let path = decode::<CachedPath>(&path)?.into_path_buf();
                let scope: Vec<u8> = row.get(2).map_err(sql_error)?;
                let scope: [u8; 32] = scope
                    .try_into()
                    .map_err(|_| "source message cache context is invalid".to_string())?;
                match fs::metadata(&path) {
                    Ok(_) => {}
                    // Test-only discovery helper: unlink absent sources from
                    // future discovery, never delete their durable facts.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if let Some(mut identity) = CacheIdentity::current_for_namespace(&namespace)
                        {
                            identity.scope = scope;
                            if let Some(stored) = read_metadata(&transaction, identity, &path)? {
                                deactivate_source(&transaction, &stored)?;
                            }
                        }
                    }
                    Err(_) => {
                        return Err("source message cache source discovery failed".to_string())
                    }
                }
            }
            drop(rows);
            drop(statement);
            transaction.commit().map_err(sql_error)?;
            flush_recovery(connection, self.root()?)
        });
        if let Err(error) = result {
            self.record_failure(error);
        }
    }

    #[cfg(test)]
    /// All source writes are already committed. This retries a pending retained
    /// recovery snapshot, never serializes dirty native history.
    pub(crate) fn save_if_dirty(&mut self) {
        let result =
            self.with_database(None, |connection| flush_recovery(connection, self.root()?));
        if let Err(error) = result {
            self.record_failure(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn visit_source_metadata<S>(
        &self,
        identity: CacheIdentity,
        sink: &mut S,
    ) -> Result<(), String>
    where
        S: FnMut(&Path, &CachedSourceMetadata) -> Result<(), String> + ?Sized,
    {
        self.with_database(Some(identity),|connection| {
            let transaction = connection.unchecked_transaction().map_err(sql_error)?;
            let mut statement = transaction.prepare("SELECT path FROM mc_sources WHERE namespace=?1 AND scope=?3 AND active=1 AND EXISTS(SELECT 1 FROM mc_versions v WHERE v.source_id=mc_sources.id AND v.generation=mc_sources.generation AND v.parser_version=?2) ORDER BY path").map_err(sql_error)?;
            let mut rows = statement.query(params![identity.namespace,identity.parser_version,identity.scope.as_slice()]).map_err(sql_error)?;
            while let Some(row) = rows.next().map_err(sql_error)? {
                let path = decode::<CachedPath>(&row.get::<_,Vec<u8>>(0).map_err(sql_error)?)?.into_path_buf();
                let metadata = read_metadata(&transaction,identity,&path)?.ok_or_else(|| "source message cache metadata disappeared".to_string())?;
                sink(&path,&metadata.metadata)?;
            }
            drop(rows);drop(statement);
            transaction.commit().map_err(sql_error)
        })
    }

    #[cfg(test)]
    pub(crate) fn entries_for_test(&self) -> Vec<CachedSourceEntry> {
        self.with_database(None,|connection| {
            let mut entries = Vec::new();
            for identity in CacheIdentity::all() {
                migrate_namespace(connection,self.root()?,identity)?;
                let mut statement = connection.prepare("SELECT path,scope FROM mc_sources WHERE namespace=?1 AND active=1 ORDER BY path,scope").map_err(sql_error)?;
                let mut rows = statement.query([identity.namespace]).map_err(sql_error)?;
                while let Some(row) = rows.next().map_err(sql_error)? {
                    let path = decode::<CachedPath>(&row.get::<_,Vec<u8>>(0).map_err(sql_error)?)?.into_path_buf();
                    let scope: Vec<u8> = row.get(1).map_err(sql_error)?;
                    let scope: [u8;32] = scope.try_into().map_err(|_| "source message cache context is invalid".to_string())?;
                    let identity = CacheIdentity { scope,..identity };
                    if let Some(entry) = materialize(connection,identity,&path,false)? { entries.push(entry); }
                }
            }
            Ok(entries)
        }).expect("valid sandboxed source cache")
    }
}

fn fingerprint_is_current(path: &Path, fingerprint: &SourceFingerprint) -> bool {
    primary_fingerprint_matches(path, fingerprint) == Some(true)
        && fingerprint.related_files.iter().all(|related| {
            let path = related.path.to_path_buf();
            if related.suffix == "-wal" {
                sqlite_wal_fingerprint(&path).as_ref() == Some(related)
            } else {
                related_file_fingerprint_matches(related, &path) == Some(true)
            }
        })
        && (fingerprint.content_hash == [0; 32]
            || hash_prefix(path, fingerprint.size) == Some(fingerprint.content_hash))
}

fn allocate_generation(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
    _parser_version: u32,
    _fingerprint: &SourceFingerprint,
    old: Option<&StoredMetadata>,
) -> Result<(i64, i64), String> {
    let generation: i64 = connection.query_row(
        "UPDATE mc_meta SET value=value+1 WHERE key='generation' AND value<9223372036854775807 RETURNING value",
        [],|row| row.get(0),
    ).map_err(sql_error)?;
    if let Some(old) = old {
        return Ok((old.id, generation));
    }
    let inactive: Option<i64> = connection
        .query_row(
            "SELECT id FROM mc_sources WHERE namespace=?1 AND scope=?2 AND path=?3",
            params![
                identity.namespace,
                identity.scope.as_slice(),
                encode_path(path)?
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(id) = inactive {
        return Ok((id, generation));
    }
    connection
        .execute(
            "INSERT INTO mc_sources(namespace,scope,path,active,generation) VALUES(?1,?2,?3,0,0)",
            params![
                identity.namespace,
                identity.scope.as_slice(),
                encode_path(path)?
            ],
        )
        .map_err(sql_error)?;
    Ok((connection.last_insert_rowid(), generation))
}

pub(crate) struct SourceRowWriter<'a> {
    connection: &'a Connection,
    source_id: i64,
    generation: i64,
    next_ordinal: u64,
    codex_incremental: Option<CodexIncrementalCache>,
    cacheable: bool,
    fallback_timestamp: i64,
}

impl SourceRowWriter<'_> {
    pub(crate) fn set_cacheable(&mut self, value: bool) -> Result<(), String> {
        self.cacheable = value;
        Ok(())
    }

    pub(crate) fn clear_codex_turn_ids(&mut self) -> Result<(), String> {
        self.connection
            .execute(
                "DELETE FROM mc_codex_turn_ids WHERE source_id=?1 AND generation=?2",
                params![self.source_id, self.generation],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub(crate) fn insert_codex_turn_id(&mut self, turn_id: &str) -> Result<(), String> {
        self.connection
            .prepare_cached("INSERT OR IGNORE INTO mc_codex_turn_ids VALUES(?1,?2,?3)")
            .map_err(sql_error)?
            .execute(params![self.source_id, self.generation, turn_id])
            .map_err(sql_error)?;
        Ok(())
    }
    pub(crate) fn push(
        &mut self,
        message: &UnifiedMessage,
        retained: bool,
        fallback: bool,
    ) -> Result<(), String> {
        let ordinal = i64::try_from(self.next_ordinal)
            .map_err(|_| "source message cache row count overflow".to_string())?;
        let bytes = encode_fact(message)?;
        let digest = Sha256::digest(&bytes);
        self.connection.prepare_cached(
            "INSERT INTO mc_rows(source_id,generation,ordinal,dedup_key,retained,fallback,timestamp,message,digest,
                 session_id,model_id,client,provider_id,workspace,workspace_label)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        ).map_err(sql_error)?.execute(params![self.source_id,self.generation,ordinal,message.dedup_key,retained,fallback,
            if fallback {self.fallback_timestamp} else {message.timestamp},bytes,digest.as_slice(),
            message.session_id,message.model_id,message.client,message.provider_id,encode_fact(&message.workspace_key)?,encode_fact(&message.workspace_label)?],
        ).map_err(sql_error)?;
        self.next_ordinal += 1;
        Ok(())
    }
}

/// Reconcile a retained row into its live same-key copy. Claude-only: every
/// other namespace returns from `merge_history` before reaching here, so this
/// delegates to the shared unit-aware policy instead of a plain max that would
/// resurrect a bare whole-prompt input over a split uncached remainder.
fn merge_message(existing: &mut UnifiedMessage, stored: &UnifiedMessage) {
    crate::sessions::claudecode::merge_claude_observations(existing, stored);
}

fn merge_history(
    writer: &mut SourceRowWriter<'_>,
    identity: CacheIdentity,
    fingerprint: &SourceFingerprint,
    produced_rows: u64,
    old: Option<&StoredMetadata>,
    refresh: &mut dyn FnMut(&mut UnifiedMessage),
) -> Result<(), String> {
    // Claude keeps unit-aware observation reconciliation for same-key rows;
    // every other keyed lane carries missing stable rows forward verbatim
    // (fresh same-key observations still win). Keyless event rows retain
    // source-local multiplicity, including equal-valued repeats.
    let filter = retained_history_key_filter(identity.namespace);
    let Some(old) = old.filter(|old| {
        // Durable readability is separate from warm admission: frozen 3/4
        // rows decode through the frozen codecs regardless of parser
        // version; only the current version is a live warm hit.
        old.format_version == CACHE_FORMAT_VERSION || matches!(old.format_version, 3 | 4)
    }) else {
        return Ok(());
    };
    // Droid rows apportion one cumulative session total, not independent
    // events. A complete nonempty snapshot supersedes its previous fragments;
    // carrying them too would double the session on metadata/weight changes.
    // Empty or missing inputs still retain the last-good snapshot.
    if identity.namespace == ClientId::Droid.as_str() && produced_rows != 0 {
        return Ok(());
    }
    let same_fingerprint = old.scope == identity.scope
        && old.parser_version == identity.parser_version
        && old.metadata.fingerprint == *fingerprint;
    // Roo-family usage rows come from ui_messages.json; sibling history only
    // annotates their model/agent. An unchanged primary cannot create new usage.
    // Replace its observed annotations, but keep previously archived UI rows.
    let primary_unchanged = produced_rows != 0
        && matches!(identity.namespace, "roocode" | "kilocode" | "cline")
        && old.scope == identity.scope
        && old.parser_version == identity.parser_version
        && old.metadata.fingerprint.size == fingerprint.size
        && old.metadata.fingerprint.modified_ns == fingerprint.modified_ns
        && old.metadata.fingerprint.sample_hashes == fingerprint.sample_hashes
        && old.metadata.fingerprint.content_hash == fingerprint.content_hash;
    let replace_annotations = primary_unchanged
        && writer
            .connection
            .query_row(
                "SELECT COUNT(*) FROM mc_rows
             WHERE source_id=?1 AND generation=?2 AND retained=0",
                params![old.id, old.metadata.generation],
                |row| row.get::<_, u64>(0),
            )
            .map_err(sql_error)?
            == produced_rows;
    let connection = writer.connection;
    let mut current_lookup = connection
        .prepare_cached(
            "SELECT ordinal,retained,message,digest FROM mc_rows
         WHERE source_id=?1 AND generation=?2 AND dedup_key=?3 ORDER BY ordinal DESC LIMIT 1",
        )
        .map_err(sql_error)?;
    let mut statement = writer
        .connection
        .prepare(
            "SELECT dedup_key,retained,message,digest FROM mc_rows
         WHERE source_id=?1 AND generation=?2 AND dedup_key IS NOT NULL ORDER BY ordinal",
        )
        .map_err(sql_error)?;
    let mut rows = statement
        .query(params![old.id, old.metadata.generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let key: String = row.get(0).map_err(sql_error)?;
        // Claude restricts retention to globally-stable keys (path-scoped
        // tool-result keys never carry); other lanes carry every stable
        // non-empty key the parser produced.
        let keep = match filter {
            Some(filter) => filter(&key),
            None => !key.is_empty(),
        };
        if !keep {
            continue;
        }
        let stored_retained: bool = row.get(1).map_err(sql_error)?;
        let bytes: Vec<u8> = row.get(2).map_err(sql_error)?;
        let digest: Vec<u8> = row.get(3).map_err(sql_error)?;
        verify_digest(&bytes, &digest)?;
        let stored = decode_stored_message(old.format_version, &bytes)?;
        let current = current_lookup
            .query_row(params![writer.source_id, writer.generation, key], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .optional()
            .map_err(sql_error)?;
        if let Some((ordinal, retained, bytes, digest)) = current {
            // Same-key reconciliation is Claude-only (unit-aware observation
            // merge). Other lanes keep the fresh parse's row untouched: the
            // producer already supplied this identity in the new generation.
            if filter.is_none() {
                continue;
            }
            // A parser upgrade cannot reconcile unlike token/context semantics.
            // Pre-parse retention carries only missing stable rows across it.
            if old.parser_version != identity.parser_version {
                continue;
            }
            verify_digest(&bytes, &digest)?;
            let mut current = decode_message(&bytes)?;
            merge_message(&mut current, &stored);
            refresh(&mut current);
            // Only producer-supplied rows can inherit a matching live observation.
            // Rows carried by this reconciliation stay retained in every history order.
            let retained = retained
                && !((ordinal as u64) < produced_rows && same_fingerprint && !stored_retained);
            let bytes = encode_fact(&current)?;
            writer.connection.prepare_cached(
                "UPDATE mc_rows SET retained=?1,message=?2,digest=?3,session_id=?7,model_id=?8,client=?9,provider_id=?10,
                     workspace=?11,workspace_label=?12 WHERE source_id=?4 AND generation=?5 AND ordinal=?6",
            ).map_err(sql_error)?.execute(params![retained,bytes,Sha256::digest(&bytes).as_slice(),writer.source_id,writer.generation,ordinal,
                current.session_id,current.model_id,current.client,current.provider_id,encode_fact(&current.workspace_key)?,encode_fact(&current.workspace_label)?],
            ).map_err(sql_error)?;
        } else {
            let mut stored = stored;
            refresh(&mut stored);
            writer.push(&stored, true, false)?;
        }
    }
    // Source-local keyless reconciliation: compare canonical decoded bytes
    // (current layout) so a frozen-3/4 vs 5 format change cannot duplicate
    // the same retained facts. Fresh keyless rows win by position; surplus
    // old rows carry verbatim; equal-valued repeats never collapse.
    let mut keyless = writer.connection.prepare(
        "SELECT message,digest,retained FROM mc_rows WHERE source_id=?1 AND generation=?2 AND dedup_key IS NULL ORDER BY ordinal",
    ).map_err(sql_error)?;
    let mut keyless_rows = keyless
        .query(params![old.id, old.metadata.generation])
        .map_err(sql_error)?;
    let mut counts_ready = false;
    while let Some(row) = keyless_rows.next().map_err(sql_error)? {
        if replace_annotations && !row.get::<_, bool>(2).map_err(sql_error)? {
            continue;
        }
        if !counts_ready {
            writer
                .connection
                .execute_batch(
                    "CREATE TEMP TABLE IF NOT EXISTS mc_keyless_matches(
                     message BLOB PRIMARY KEY,remaining INTEGER NOT NULL) WITHOUT ROWID;
                 DELETE FROM mc_keyless_matches;",
                )
                .map_err(sql_error)?;
            writer
                .connection
                .execute(
                    "INSERT INTO mc_keyless_matches
                 SELECT message,COUNT(*) FROM mc_rows
                 WHERE source_id=?1 AND generation=?2 AND dedup_key IS NULL GROUP BY message",
                    params![writer.source_id, writer.generation],
                )
                .map_err(sql_error)?;
            counts_ready = true;
        }
        let bytes: Vec<u8> = row.get(0).map_err(sql_error)?;
        let digest: Vec<u8> = row.get(1).map_err(sql_error)?;
        verify_digest(&bytes, &digest)?;
        let stored = decode_stored_message(old.format_version, &bytes)?;
        let canonical = if old.format_version == CACHE_FORMAT_VERSION {
            std::borrow::Cow::Borrowed(bytes.as_slice())
        } else {
            std::borrow::Cow::Owned(encode_fact(&stored)?)
        };
        // Consume one occurrence; counters spill to disk rather than keeping
        // every keyless payload and a quadratic match scan in memory.
        let consumed = writer
            .connection
            .execute(
                "UPDATE mc_keyless_matches SET remaining=remaining-1
             WHERE message=?1 AND remaining>0",
                [canonical.as_ref()],
            )
            .map_err(sql_error)?;
        if consumed != 0 {
            continue;
        }
        let mut stored = stored;
        refresh(&mut stored);
        writer.push(&stored, true, false)?;
    }
    Ok(())
}

fn retained_digest(connection: &Connection, id: i64, generation: i64) -> Result<Vec<u8>, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"tokscale-retained-source-v2");
    let mut statement = connection.prepare(
        "SELECT dedup_key,digest FROM mc_rows WHERE source_id=?1 AND generation=?2 AND retained=1
         ORDER BY dedup_key,digest,ordinal",
    ).map_err(sql_error)?;
    let mut rows = statement
        .query(params![id, generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let key: Option<String> = row.get(0).map_err(sql_error)?;
        let digest: Vec<u8> = row.get(1).map_err(sql_error)?;
        if digest.len() != 32 {
            return Err("source message cache row digest is invalid".to_string());
        }
        let key = key.unwrap_or_default();
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update(digest);
    }
    Ok(hasher.finalize().to_vec())
}

#[allow(clippy::too_many_arguments)]
fn publish_generation(
    connection: &Connection,
    identity: CacheIdentity,
    id: i64,
    generation: i64,
    fingerprint: &SourceFingerprint,
    resume: Option<&CodexIncrementalCache>,
    cacheable: bool,
    advance_current: bool,
    old: Option<&StoredMetadata>,
) -> Result<CachedSourceMetadata, String> {
    let (message_count,retained_count,fallback_count): (u64,u64,u64) = connection.query_row(
        "SELECT count(*),coalesce(sum(retained),0),coalesce(sum(fallback),0) FROM mc_rows WHERE source_id=?1 AND generation=?2",
        params![id,generation],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).map_err(sql_error)?;
    let digest = retained_digest(connection, id, generation)?;
    let (codex_turn_count, codex_turn_digest) = codex_ids_digest(connection, id, generation)?;
    if advance_current && old.map_or(retained_count > 0, |old| old.retained_digest != digest) {
        connection
            .execute(
                "UPDATE mc_meta SET value=value+1 WHERE key='retained_revision'",
                [],
            )
            .map_err(sql_error)?;
    }
    connection.execute(
        "INSERT INTO mc_versions(source_id,generation,parser_version,format_version,fingerprint,resume,
             message_count,retained_count,fallback_count,retained_digest,cacheable,codex_turn_count,codex_turn_digest)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![id,generation,identity.parser_version,CACHE_FORMAT_VERSION,encode(fingerprint)?,resume.map(encode).transpose()?,
                message_count,retained_count,fallback_count,digest,cacheable,codex_turn_count,codex_turn_digest],
    ).map_err(sql_error)?;
    if advance_current {
        connection
            .execute(
                "UPDATE mc_sources SET active=1,generation=?1 WHERE id=?2",
                params![generation, id],
            )
            .map_err(sql_error)?;
    }
    Ok(CachedSourceMetadata {
        source_id: id,
        fingerprint: fingerprint.clone(),
        generation,
        cacheable,
        message_count,
        retained_count,
        fallback_count,
        codex_incremental: resume.cloned(),
        legacy_history: None,
    })
}

fn flush_recovery(connection: &Connection, root: &Path) -> Result<(), String> {
    let (revision,recovered): (i64,i64) = connection.query_row(
        "SELECT (SELECT value FROM mc_meta WHERE key='retained_revision'),(SELECT value FROM mc_meta WHERE key='recovery_revision')",
        [],|row| Ok((row.get(0)?,row.get(1)?)),
    ).map_err(sql_error)?;
    if revision == recovered {
        return Ok(());
    }
    crate::usage_store::refresh_retained_recovery(connection, root)?;
    connection
        .execute(
            "UPDATE mc_meta SET value=?1 WHERE key='recovery_revision' AND value<?1",
            [revision],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn verify_digest(bytes: &[u8], digest: &[u8]) -> Result<(), String> {
    if digest != Sha256::digest(bytes).as_slice() {
        return Err("source message cache row digest is invalid".to_string());
    }
    Ok(())
}

fn visit_stored_rows<S>(
    connection: &Connection,
    stored: &StoredMetadata,
    sink: &mut S,
) -> Result<(), String>
where
    S: FnMut(&UnifiedMessage, bool, bool) -> Result<(), String> + ?Sized,
{
    let mut statement = connection.prepare("SELECT ordinal,retained,fallback,message,digest,timestamp FROM mc_rows WHERE source_id=?1 AND generation=?2 ORDER BY ordinal").map_err(sql_error)?;
    let mut rows = statement
        .query(params![stored.id, stored.metadata.generation])
        .map_err(sql_error)?;
    let mut count = 0;
    let mut retained_count = 0;
    let mut fallback_count = 0;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let ordinal: u64 = row.get(0).map_err(sql_error)?;
        let retained: bool = row.get(1).map_err(sql_error)?;
        let fallback: bool = row.get(2).map_err(sql_error)?;
        let bytes: Vec<u8> = row.get(3).map_err(sql_error)?;
        let digest: Vec<u8> = row.get(4).map_err(sql_error)?;
        if ordinal != count {
            return Err("source message cache source rows are incomplete".to_string());
        }
        verify_digest(&bytes, &digest)?;
        let mut message = decode_stored_message(stored.format_version, &bytes)?;
        if fallback {
            message.set_timestamp(row.get(5).map_err(sql_error)?);
        }
        sink(&message, retained, fallback)?;
        count += 1;
        retained_count += u64::from(retained);
        fallback_count += u64::from(fallback);
    }
    if count != stored.metadata.message_count
        || retained_count != stored.metadata.retained_count
        || fallback_count != stored.metadata.fallback_count
    {
        return Err("source message cache source rows are incomplete".to_string());
    }
    Ok(())
}

fn legacy_history_exists(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
) -> Result<bool, String> {
    if identity.scope == [0; 32] || retained_history_key_filter(identity.namespace).is_none() {
        return Ok(false);
    }
    let Some(stored) = read_metadata(
        connection,
        CacheIdentity {
            scope: [0; 32],
            ..identity
        },
        path,
    )?
    else {
        return Ok(false);
    };
    if !admitted(identity, stored.parser_version) {
        return Ok(false);
    }
    // A scoped transfer consumes the legacy discovery pointer, not its facts.
    // A later legacy writer reactivates the pointer and requires another merge.
    connection
        .query_row(
            "SELECT active FROM mc_sources WHERE id=?1",
            [stored.id],
            |row| row.get(0),
        )
        .map_err(sql_error)
}

fn latest_durable_history(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
) -> Result<Option<StoredMetadata>, String> {
    // Warm reuse can be unsafe even for valid collected facts (for example,
    // Codex EOF without a newline). Preserve the latest published generation.
    let version: Option<(i64, i64)> = connection
        .query_row(
            "SELECT s.id,v.generation FROM mc_sources s JOIN mc_versions v ON v.source_id=s.id
         WHERE s.namespace=?1 AND s.scope=?2 AND s.path=?3 AND v.generation<=s.generation
         ORDER BY v.generation DESC LIMIT 1",
            params![
                identity.namespace,
                identity.scope.as_slice(),
                encode_path(path)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    version
        .map(|(id, generation)| read_version(connection, id, generation))
        .transpose()
}

#[cfg(test)]
fn materialize(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
    current_only: bool,
) -> Result<Option<CachedSourceEntry>, String> {
    let transaction = connection.unchecked_transaction().map_err(sql_error)?;
    if current_only && legacy_history_exists(&transaction, identity, path)? {
        return Ok(None);
    }
    let mut stored = if current_only {
        read_metadata(&transaction, identity, path)?
    } else {
        latest_durable_history(&transaction, identity, path)?
    };
    if stored.is_none() && !current_only && identity.scope != [0; 32] {
        stored = read_metadata(
            &transaction,
            CacheIdentity {
                scope: [0; 32],
                ..identity
            },
            path,
        )?;
    }
    let Some(stored) = stored.filter(|stored| {
        if current_only {
            stored.format_version == CACHE_FORMAT_VERSION
                && stored.parser_version == identity.parser_version
                && stored.metadata.cacheable
        } else {
            true
        }
    }) else {
        return Ok(None);
    };
    transaction
        .execute(
            "INSERT OR IGNORE INTO mc_observed VALUES(?1,?2)",
            params![stored.id, stored.metadata.generation],
        )
        .map_err(sql_error)?;
    let mut entry = CachedSourceEntry {
        parser_namespace: identity.namespace.to_string(),
        parser_version: stored.parser_version,
        scope: stored.scope,
        path: CachedPath::from_path(path),
        fingerprint: stored.metadata.fingerprint.clone(),
        messages: Vec::new(),
        fallback_timestamp_indices: Vec::new(),
        codex_incremental: stored.metadata.codex_incremental.clone(),
        retained_keys: HashSet::new(),
    };
    visit_stored_rows(
        &transaction,
        &stored,
        &mut |message: &UnifiedMessage, retained, fallback| {
            if retained {
                if let Some(key) = message.dedup_key.as_ref() {
                    entry.retained_keys.insert(key.clone());
                }
            }
            if fallback {
                entry.fallback_timestamp_indices.push(entry.messages.len());
            }
            entry.messages.push(message.clone());
            Ok(())
        },
    )?;
    if let Some(resume) = entry.codex_incremental.as_mut() {
        visit_codex_ids(&transaction, &stored, &mut |id: &str| {
            resume
                .state
                .forked_child_task_started_turn_ids
                .insert(id.to_string());
            Ok(())
        })?;
    }
    transaction.commit().map_err(sql_error)?;
    Ok(Some(entry))
}

/// Durable facts are never deactivated by source absence: the row set stays
/// canonical and queryable. `active=0` now only unlinks a source from future
/// scope discovery; selection keeps serving its last-good generation until a
/// newer generation for the same source identity supersedes it.
fn deactivate_source(connection: &Connection, stored: &StoredMetadata) -> Result<(), String> {
    if stored.metadata.retained_count > 0 {
        connection
            .execute(
                "UPDATE mc_meta SET value=value+1 WHERE key='retained_revision'",
                [],
            )
            .map_err(sql_error)?;
    }
    connection
        .execute(
            "UPDATE mc_sources SET active=0 WHERE id=?1 AND generation=?2",
            params![stored.id, stored.metadata.generation],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn delete_source(connection: &Connection, stored: &StoredMetadata) -> Result<(), String> {
    deactivate_source(connection, stored)
}

pub(crate) fn retained_state_change_token(
    cache_root: Option<&Path>,
    source_paths: &[PathBuf],
    home: &Path,
) -> Result<u64, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"tokscale-retained-state-v2");
    let Some(root) = cache_root else {
        return Ok(u64::from_le_bytes(
            hasher.finalize()[..8].try_into().expect("SHA256 prefix"),
        ));
    };
    // An absent store has no published retained state. Do not create the cache
    // merely to probe it, but existing legacy shards must be migrated losslessly.
    let exists = |path: &Path| -> Result<bool, String> {
        match fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("source message cache probe failed".to_string()),
        }
    };
    if !exists(&root.join("source-message-cache.sqlite3"))?
        && !exists(&root.join(CACHE_SHARD_DIRNAME))?
    {
        return Ok(u64::from_le_bytes(
            hasher.finalize()[..8].try_into().expect("SHA256 prefix"),
        ));
    }
    let cache = SourceMessageCache::open_from_root(Some(root));
    let identity = CacheIdentity::for_client(ClientId::Claude).with_scope(home);
    cache.with_database(Some(identity), |connection| {
        let transaction = connection.unchecked_transaction().map_err(sql_error)?;
        for (index, path) in source_paths.iter().enumerate() {
            for scope in [identity.scope, [0; 32]] {
                let scoped = CacheIdentity { scope, ..identity };
                if let Some(stored) = read_metadata(&transaction, scoped, path)?.filter(|stored| {
                    admitted(identity, stored.parser_version)
                        && (stored.metadata.retained_count > 0
                            || scope == [0; 32] && stored.metadata.message_count > 0)
                }) {
                    hasher.update((index as u64).to_le_bytes());
                    hasher.update(scope);
                    update_path_digest(path, &mut hasher);
                    hasher.update(stored.retained_digest);
                    if scope == [0; 32] {
                        hasher.update(stored.metadata.generation.to_le_bytes());
                    }
                }
            }
        }
        transaction.commit().map_err(sql_error)
    })?;
    Ok(u64::from_le_bytes(
        hasher.finalize()[..8].try_into().expect("SHA256 prefix"),
    ))
}

#[cfg(test)]
pub(crate) fn set_parser_version_for_test(identity: CacheIdentity, version: u32) {
    let cache = SourceMessageCache::load();
    cache.with_database(Some(identity),|connection| {
        connection.execute("UPDATE mc_versions SET parser_version=?1 WHERE source_id IN(SELECT id FROM mc_sources WHERE namespace=?2)",params![version,identity.namespace]).map_err(sql_error)?;
        Ok(())
    }).expect("valid sandboxed cache");
}

/// The old envelope is decoded as a stream: its byte-vector payload never
/// becomes a native Vec, nor does an entry's message vector. The whole shard
/// transfer and its completion marker are one SQLite transaction.
fn legacy_directory_signature(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("source message cache legacy directory is unreadable".to_string()),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("source message cache legacy directory is invalid".to_string());
    }
    let modified = metadata
        .modified()
        .map_err(|_| "source message cache legacy directory is unreadable".to_string())?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "source message cache legacy directory time is invalid".to_string())?
        .as_nanos();
    let mut signature = modified.to_le_bytes().to_vec();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        signature.extend_from_slice(&metadata.dev().to_le_bytes());
        signature.extend_from_slice(&metadata.ino().to_le_bytes());
        signature.extend_from_slice(&metadata.ctime().to_le_bytes());
        signature.extend_from_slice(&metadata.ctime_nsec().to_le_bytes());
    }
    #[cfg(not(unix))]
    if let Ok(created) = metadata.created().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .map_err(std::io::Error::other)
    }) {
        signature.extend_from_slice(&created.as_nanos().to_le_bytes());
    }
    Ok(signature)
}

fn migrate_namespace(
    connection: &Connection,
    root: &Path,
    identity: CacheIdentity,
) -> Result<(), String> {
    let identity = CacheIdentity {
        scope: [0; 32],
        ..identity
    };
    let directory = root.join(CACHE_SHARD_DIRNAME).join(identity.namespace);
    let signature = legacy_directory_signature(&directory)?;
    let previous: Option<Vec<u8>> = connection
        .query_row(
            "SELECT signature FROM mc_namespaces WHERE namespace=?1",
            [identity.namespace],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if previous.as_ref() == Some(&signature) {
        return Ok(());
    }
    if !signature.is_empty() {
        let lock_path = root.join(CACHE_LOCK_FILENAME);
        if fs::symlink_metadata(&lock_path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err("source message cache legacy lock is invalid".to_string());
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|_| "source message cache legacy lock is unavailable".to_string())?;
        fs2::FileExt::lock_exclusive(&lock)
            .map_err(|_| "source message cache legacy lock failed".to_string())?;
        for entry in fs::read_dir(&directory)
            .map_err(|_| "source message cache legacy directory is unreadable".to_string())?
        {
            let entry =
                entry.map_err(|_| "source message cache legacy discovery failed".to_string())?;
            let Some(index) = parse_shard_filename(&entry.file_name()) else {
                continue;
            };
            migrate_shard(connection, root, identity, index, &entry.path())?;
        }
        release_missing_shard_pins(connection, &directory)?;
        // Sample after our own retirements, while the legacy writer lock still
        // excludes replacement. A later old writer changes this token again.
        let signature = legacy_directory_signature(&directory)?;
        connection
            .execute(
                "INSERT OR REPLACE INTO mc_namespaces VALUES(?1,?2)",
                params![identity.namespace, signature],
            )
            .map_err(sql_error)?;
        drop(lock);
    } else {
        connection
            .execute(
                "INSERT OR REPLACE INTO mc_namespaces VALUES(?1,?2)",
                params![identity.namespace, signature],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

/// Import legacy file shards from `legacy_root` into an already-open store
/// handle without retiring anything: no shard unlink, no recovery write, no
/// missing-pin release. Reuses the shared transfer codecs for every known
/// namespace (including `synthetic`) and all supported frozen layouts
/// (3/4/5). Records `mc_migrations` evidence plus `mc_namespaces`
/// signatures, so repeated pre-activation calls are no-ops. The caller owns
/// activation and owns `connection`; shard-only staged DBs get their mc
/// schema (and store appID/user_version via the shared opener path) ensured
/// here through the existing `initialize` reuse.
pub(crate) fn import_legacy_shards_on(
    connection: &Connection,
    legacy_root: &Path,
) -> Result<(), String> {
    initialize(connection)?;
    // `migrate_namespace` scopes to `[0;32]` internally; the scoped Claude
    // history migrates through the same path on first write. Enumerate every
    // namespace with a parser owner, plus the synthetic lane.
    let mut namespaces: Vec<CacheIdentity> = crate::ClientId::iter()
        .map(CacheIdentity::for_client)
        .collect();
    namespaces.push(CacheIdentity::synthetic());
    for identity in namespaces {
        let identity = CacheIdentity {
            scope: [0; 32],
            ..identity
        };
        let directory = legacy_root
            .join(CACHE_SHARD_DIRNAME)
            .join(identity.namespace);
        let signature = legacy_directory_signature(&directory)?;
        let previous: Option<Vec<u8>> = connection
            .query_row(
                "SELECT signature FROM mc_namespaces WHERE namespace=?1",
                [identity.namespace],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if previous.as_ref() == Some(&signature) {
            continue;
        }
        if !signature.is_empty() {
            for entry in std::fs::read_dir(&directory)
                .map_err(|_| "source message cache legacy directory is unreadable".to_string())?
            {
                let entry = entry
                    .map_err(|_| "source message cache legacy discovery failed".to_string())?;
                let Some(index) =
                    crate::message_cache::parse_shard_filename_for_import(&entry.file_name())
                else {
                    continue;
                };
                // Shared transfer + verify + evidence; never retires. Valid
                // empty receipts (cold shard, no transferable rows) are kept
                // as evidence, never required to hold rows.
                let _ = transfer_shard_rows(connection, identity, index, &entry.path())?;
            }
            connection
                .execute(
                    "INSERT OR REPLACE INTO mc_namespaces VALUES(?1,?2)",
                    params![identity.namespace, signature],
                )
                .map_err(sql_error)?;
        } else {
            connection
                .execute(
                    "INSERT OR REPLACE INTO mc_namespaces VALUES(?1,?2)",
                    params![identity.namespace, signature],
                )
                .map_err(sql_error)?;
        }
    }
    Ok(())
}

fn migrate_shard(
    connection: &Connection,
    root: &Path,
    identity: CacheIdentity,
    index: usize,
    path: &Path,
) -> Result<(), String> {
    let Some((before, digest, source_count)) =
        transfer_shard_rows(connection, identity, index, path)?
    else {
        return Ok(());
    };
    retire_verified_shard(connection, root, path, &before, &digest, source_count)
}

fn transfer_shard_rows(
    connection: &Connection,
    identity: CacheIdentity,
    index: usize,
    path: &Path,
) -> Result<Option<(SourceFingerprint, [u8; 32], u64)>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "source message cache legacy shard is unreadable".to_string())?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "source message cache legacy shard is unreadable".to_string())?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_CACHE_SHARD_BYTES
    {
        return Err("source message cache legacy shard size is invalid".to_string());
    }
    let before = SourceFingerprint::from_path_samples_only(path)
        .ok_or_else(|| "source message cache legacy shard is unreadable".to_string())?;
    let digest = hash_prefix(path, metadata.len())
        .ok_or_else(|| "source message cache legacy shard is unreadable".to_string())?;
    let encoded_path = encode_path(path)?;
    let transferred: Option<u64> = connection
        .query_row(
            "SELECT source_count FROM mc_migrations WHERE path=?1 AND digest=?2",
            params![encoded_path, digest.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(source_count) = transferred {
        verify_migration_transfer(connection, &encoded_path, &digest, source_count)?;
        return Ok(Some((before, digest, source_count)));
    }
    let mut reader = BufReader::new(&mut file);
    let format_version: u32 = bincode::options()
        .with_limit(MAX_CACHE_SHARD_BYTES)
        .deserialize_from(&mut reader)
        .map_err(|_| "source message cache legacy envelope is invalid".to_string())?;
    let namespace: String = bincode::options()
        .with_limit(MAX_CACHE_SHARD_BYTES)
        .deserialize_from(&mut reader)
        .map_err(|_| "source message cache legacy envelope is invalid".to_string())?;
    let version: u32 = bincode::options()
        .with_limit(MAX_CACHE_SHARD_BYTES)
        .deserialize_from(&mut reader)
        .map_err(|_| "source message cache legacy envelope is invalid".to_string())?;
    if namespace != identity.namespace {
        return Err("source message cache legacy parser owner is invalid".to_string());
    }
    let supported_format =
        format_version == 3 || format_version == 4 || format_version == CACHE_FORMAT_VERSION;
    if !supported_format || !admitted(identity, version) {
        // Known pre-retention layouts and non-retaining parser misses are cold,
        // but never delete a newer/unknown retaining layout we cannot interpret.
        if retained_history_key_filter(identity.namespace).is_some()
            && (format_version > CACHE_FORMAT_VERSION || version > identity.parser_version)
        {
            return Err("source message cache legacy retained layout is unsupported".to_string());
        }
        return Ok(None);
    }
    let payload_len: u64 = bincode::options()
        .with_limit(MAX_CACHE_SHARD_BYTES)
        .deserialize_from(&mut reader)
        .map_err(|_| "source message cache legacy envelope is invalid".to_string())?;
    if payload_len > MAX_CACHE_SHARD_BYTES {
        return Err("source message cache legacy payload size is invalid".to_string());
    }
    let transaction = connection.unchecked_transaction().map_err(sql_error)?;
    transaction
        .execute(
            "UPDATE mc_meta SET value=value WHERE key='schema_version'",
            [],
        )
        .map_err(sql_error)?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS mc_imported_sources(source_id INTEGER PRIMARY KEY);
         DELETE FROM mc_imported_sources;",
        )
        .map_err(sql_error)?;
    let mut payload = reader.by_ref().take(payload_len);
    bincode::options()
        .with_limit(payload_len)
        .deserialize_from_seed(
            LegacyEntries {
                connection: &transaction,
                identity,
                index,
                format_version,
            },
            &mut payload,
        )
        .map_err(|_| "source message cache legacy payload transfer failed".to_string())?;
    if payload.limit() != 0 {
        return Err("source message cache legacy payload has trailing bytes".to_string());
    }
    let mut extra = [0_u8; 1];
    if reader
        .read(&mut extra)
        .map_err(|_| "source message cache legacy shard read failed".to_string())?
        != 0
    {
        return Err("source message cache legacy envelope has trailing bytes".to_string());
    }
    drop(reader);
    drop(file);
    // Verify every transferred row and metadata before recording completion.
    // A migration failure leaves both the old shard and the previous DB intact.
    verify_shard_transfer(&transaction, identity)?;
    if !fingerprint_is_current(path, &before) || hash_prefix(path, metadata.len()) != Some(digest) {
        return Err("source message cache legacy shard changed during migration".to_string());
    }
    let source_count =
        record_shard_transfer(&transaction, &encoded_path, &digest, identity.namespace)?;
    transaction.commit().map_err(sql_error)?;
    Ok(Some((before, digest, source_count)))
}

/// Verify every row a shard transfer imported. Shared by the retiring live
/// migration and the non-retiring pre-activation importer.
fn verify_shard_transfer(transaction: &Connection, identity: CacheIdentity) -> Result<(), String> {
    let mut imported = transaction.prepare(
        "SELECT s.path FROM mc_sources s JOIN mc_imported_sources i ON i.source_id=s.id ORDER BY s.id",
    ).map_err(sql_error)?;
    let mut rows = imported.query([]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let source =
            decode::<CachedPath>(&row.get::<_, Vec<u8>>(0).map_err(sql_error)?)?.into_path_buf();
        let stored = read_metadata(transaction, identity, &source)?
            .ok_or_else(|| "source message cache migration is incomplete".to_string())?;
        validate_stored_generation(transaction, &stored)?;
    }
    Ok(())
}

/// Record `mc_migrations` evidence for a verified transfer and bump the
/// retained revision for retaining namespaces. Returns the source count.
/// Shared by the retiring live migration and the non-retiring importer.
fn record_shard_transfer(
    transaction: &Connection,
    encoded_path: &[u8],
    digest: &[u8],
    namespace: &str,
) -> Result<u64, String> {
    let source_count: u64 = transaction
        .query_row("SELECT count(*) FROM mc_imported_sources", [], |row| {
            row.get(0)
        })
        .map_err(sql_error)?;
    transaction
        .execute(
            "INSERT OR REPLACE INTO mc_migrations(path,digest,source_count) VALUES(?1,?2,?3)",
            params![encoded_path, digest, source_count],
        )
        .map_err(sql_error)?;
    transaction.execute(
        "INSERT INTO mc_migration_versions(path,digest,source_id,generation)
         SELECT ?1,?2,s.id,s.generation FROM mc_sources s JOIN mc_imported_sources i ON i.source_id=s.id",
        params![encoded_path,digest],
    ).map_err(sql_error)?;
    if retained_history_key_filter(namespace).is_some() {
        // Imported Claude history may already be absent from its current source
        // even when the legacy label said live. Preserve it before retiring the
        // last shard, without changing the label used by downstream selection.
        transaction
            .execute(
                "UPDATE mc_meta SET value=value+1 WHERE key='retained_revision'",
                [],
            )
            .map_err(sql_error)?;
    }
    Ok(source_count)
}
struct LegacyEntries<'a> {
    connection: &'a Connection,
    identity: CacheIdentity,
    index: usize,
    format_version: u32,
}

impl<'de> DeserializeSeed<'de> for LegacyEntries<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for LegacyEntries<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("legacy source entries")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence
            .next_element_seed(LegacyEntry {
                connection: self.connection,
                identity: self.identity,
                index: self.index,
                format_version: self.format_version,
            })?
            .is_some()
        {}
        Ok(())
    }
}

struct LegacyEntry<'a> {
    connection: &'a Connection,
    identity: CacheIdentity,
    index: usize,
    format_version: u32,
}

impl<'de> DeserializeSeed<'de> for LegacyEntry<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_tuple(8, self)
    }
}

fn legacy_field<'de, T: Deserialize<'de>, A: SeqAccess<'de>>(
    sequence: &mut A,
) -> Result<T, A::Error> {
    sequence
        .next_element()?
        .ok_or_else(|| serde::de::Error::custom("legacy source entry is incomplete"))
}

impl<'de> Visitor<'de> for LegacyEntry<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy source entry")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        let namespace: String = legacy_field(&mut sequence)?;
        let version: u32 = legacy_field(&mut sequence)?;
        let path: CachedPath = legacy_field(&mut sequence)?;
        let fingerprint: SourceFingerprint = legacy_field(&mut sequence)?;
        if namespace != self.identity.namespace
            || !admitted(self.identity, version)
            || legacy_shard_index(self.identity.namespace, &path) != self.index
        {
            return Err(serde::de::Error::custom(
                "legacy source identity is invalid",
            ));
        }
        let path = path.into_path_buf();
        let old = read_metadata(self.connection, self.identity, &path)
            .map_err(serde::de::Error::custom)?;
        if let Some(old) = old.as_ref() {
            validate_stored_generation(self.connection, old).map_err(serde::de::Error::custom)?;
        }
        let (id, generation) = allocate_generation(
            self.connection,
            self.identity,
            &path,
            version,
            &fingerprint,
            old.as_ref(),
        )
        .map_err(serde::de::Error::custom)?;
        self.connection
            .execute("INSERT INTO mc_imported_sources VALUES(?1)", [id])
            .map_err(serde::de::Error::custom)?;
        let mut writer = SourceRowWriter {
            connection: self.connection,
            source_id: id,
            generation,
            next_ordinal: 0,
            codex_incremental: None,
            cacheable: true,
            fallback_timestamp: (fingerprint.modified_ns / 1_000_000) as i64,
        };
        let messages = if self.format_version == 3 {
            sequence.next_element_seed(StreamSequence::<format3::UnifiedMessage, _>::new(
                |message: format3::UnifiedMessage| writer.push(&message.into(), false, false),
            ))?
        } else if self.format_version == 4 {
            sequence.next_element_seed(StreamSequence::<format4::UnifiedMessage, _>::new(
                |message: format4::UnifiedMessage| writer.push(&message.into(), false, false),
            ))?
        } else {
            sequence.next_element_seed(StreamSequence::<UnifiedMessage, _>::new(|message| {
                writer.push(&message, false, false)
            }))?
        };
        if messages.is_none() {
            return Err(serde::de::Error::custom(
                "legacy source messages are missing",
            ));
        }
        if sequence.next_element_seed(StreamSequence::<usize,_>::new(|index| {
            if index as u64 >= writer.next_ordinal { return Err("legacy fallback index is invalid".to_string()); }
            self.connection.prepare_cached(
                "UPDATE mc_rows SET fallback=1,timestamp=?4 WHERE source_id=?1 AND generation=?2 AND ordinal=?3",
            ).map_err(sql_error)?.execute(params![id,generation,index as i64,writer.fallback_timestamp],
            ).map_err(sql_error)?;Ok(())
        }))?.is_none() { return Err(serde::de::Error::custom("legacy fallback indices are missing")); }
        writer.codex_incremental = sequence
            .next_element_seed(LegacyCodexIncremental {
                connection: self.connection,
                source_id: id,
                generation,
            })?
            .ok_or_else(|| serde::de::Error::custom("legacy resume metadata is missing"))?;
        writer.cacheable = self.identity.namespace != ClientId::Codex.as_str()
            || writer.codex_incremental.as_ref().is_some_and(|resume| {
                resume.consumed_offset == fingerprint.size
                    && resume.ends_with_newline
                    && resume.prefix_hash == fingerprint.content_hash
            });
        if sequence.next_element_seed(StreamSequence::<String,_>::new(|key| {
            self.connection.prepare_cached(
                "UPDATE mc_rows SET retained=1 WHERE source_id=?1 AND generation=?2 AND dedup_key=?3",
            ).map_err(sql_error)?.execute(params![id,generation,key],
            ).map_err(sql_error)?;Ok(())
        }))?.is_none() { return Err(serde::de::Error::custom("legacy retained keys are missing")); }
        let legacy_identity = CacheIdentity {
            parser_version: version,
            ..self.identity
        };
        if let Some(old) = old.as_ref() {
            let imported = StoredMetadata {
                id,
                parser_version: version,
                format_version: CACHE_FORMAT_VERSION,
                scope: self.identity.scope,
                retained_digest: Vec::new(),
                codex_turn_count: 0,
                codex_turn_digest: Vec::new(),
                metadata: CachedSourceMetadata {
                    source_id: id,
                    fingerprint,
                    generation,
                    cacheable: true,
                    message_count: writer.next_ordinal,
                    retained_count: 0,
                    fallback_count: 0,
                    codex_incremental: writer.codex_incremental.clone(),
                    legacy_history: None,
                },
            };
            let (_, current_generation) = allocate_generation(
                self.connection,
                self.identity,
                &path,
                old.parser_version,
                &old.metadata.fingerprint,
                Some(old),
            )
            .map_err(serde::de::Error::custom)?;
            self.connection.execute(
                    "INSERT INTO mc_rows SELECT source_id,?1,ordinal,dedup_key,retained,fallback,timestamp,message,digest,
                         session_id,model_id,client,provider_id,workspace,workspace_label
                     FROM mc_rows WHERE source_id=?2 AND generation=?3",
                    params![current_generation,id,old.metadata.generation],
                ).map_err(serde::de::Error::custom)?;
            self.connection
                .execute(
                    "INSERT OR IGNORE INTO mc_codex_turn_ids SELECT source_id,?1,turn_id
                     FROM mc_codex_turn_ids WHERE source_id=?2 AND generation IN (?3,?4)",
                    params![current_generation, id, old.metadata.generation, generation],
                )
                .map_err(serde::de::Error::custom)?;
            migrate_copied_rows(self.connection, id, current_generation, old.format_version)
                .map_err(serde::de::Error::custom)?;
            let mut current = SourceRowWriter {
                connection: self.connection,
                source_id: id,
                generation: current_generation,
                next_ordinal: old.metadata.message_count,
                codex_incremental: old.metadata.codex_incremental.clone(),
                cacheable: old.metadata.cacheable,
                fallback_timestamp: (old.metadata.fingerprint.modified_ns / 1_000_000) as i64,
            };
            let current_identity = CacheIdentity {
                parser_version: old.parser_version,
                ..self.identity
            };
            merge_history(
                &mut current,
                current_identity,
                &old.metadata.fingerprint,
                old.metadata.message_count,
                Some(&imported),
                &mut |message| {
                    if self.identity.namespace == ClientId::Claude.as_str() {
                        crate::sessions::claudecode::refresh_retained_message_context(
                            message, &path, None, None,
                        );
                    }
                },
            )
            .map_err(serde::de::Error::custom)?;
            publish_generation(
                self.connection,
                current_identity,
                id,
                current_generation,
                &old.metadata.fingerprint,
                current.codex_incremental.as_ref(),
                current.cacheable,
                true,
                Some(old),
            )
            .map_err(serde::de::Error::custom)?;
            self.connection
                .execute(
                    "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2",
                    params![id, generation],
                )
                .map_err(serde::de::Error::custom)?;
            self.connection
                .execute(
                    "DELETE FROM mc_codex_turn_ids WHERE source_id=?1 AND generation=?2",
                    params![id, generation],
                )
                .map_err(serde::de::Error::custom)?;
        } else {
            publish_generation(
                self.connection,
                legacy_identity,
                id,
                generation,
                &fingerprint,
                writer.codex_incremental.as_ref(),
                writer.cacheable,
                true,
                None,
            )
            .map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

struct StreamSequence<T, F> {
    callback: F,
    marker: PhantomData<T>,
}

impl<T, F> StreamSequence<T, F> {
    fn new(callback: F) -> Self {
        Self {
            callback,
            marker: PhantomData,
        }
    }
}

impl<'de, T: Deserialize<'de>, F: FnMut(T) -> Result<(), String>> DeserializeSeed<'de>
    for StreamSequence<T, F>
{
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de, T: Deserialize<'de>, F: FnMut(T) -> Result<(), String>> Visitor<'de>
    for StreamSequence<T, F>
{
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("streamed legacy sequence")
    }
    fn visit_seq<A: SeqAccess<'de>>(mut self, mut sequence: A) -> Result<(), A::Error> {
        while let Some(value) = sequence.next_element()? {
            (self.callback)(value).map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

pub(crate) fn recover_source_message_cache(cache_root: &Path) -> Result<(), String> {
    crate::usage_store::recover_database(cache_root)
}

#[cfg(test)]
fn validate_stored_counts(connection: &Connection, stored: &StoredMetadata) -> Result<(), String> {
    let (count,retained,fallback,first,last): (u64,u64,u64,Option<u64>,Option<u64>) = connection.query_row(
        "SELECT count(*),coalesce(sum(retained),0),coalesce(sum(fallback),0),min(ordinal),max(ordinal)
         FROM mc_rows WHERE source_id=?1 AND generation=?2",
        params![stored.id,stored.metadata.generation],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).map_err(sql_error)?;
    if count != stored.metadata.message_count
        || retained != stored.metadata.retained_count
        || fallback != stored.metadata.fallback_count
        || (count > 0 && (first != Some(0) || last != Some(count - 1)))
    {
        return Err("source message cache source rows are incomplete".to_string());
    }
    Ok(())
}

fn visit_codex_ids<S>(
    connection: &Connection,
    stored: &StoredMetadata,
    sink: &mut S,
) -> Result<(), String>
where
    S: FnMut(&str) -> Result<(), String> + ?Sized,
{
    let mut hasher = Sha256::new();
    hasher.update(b"tokscale-codex-turn-membership-v1");
    let mut count = 0;
    let mut statement = connection.prepare("SELECT turn_id FROM mc_codex_turn_ids WHERE source_id=?1 AND generation=?2 ORDER BY turn_id").map_err(sql_error)?;
    let mut rows = statement
        .query(params![stored.id, stored.metadata.generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let id: String = row.get(0).map_err(sql_error)?;
        hasher.update((id.len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
        count += 1;
        sink(&id)?;
    }
    if count != stored.codex_turn_count || hasher.finalize().as_slice() != stored.codex_turn_digest
    {
        return Err("source message cache Codex resume membership is incomplete".to_string());
    }
    Ok(())
}

struct LegacyCodexIncremental<'a> {
    connection: &'a Connection,
    source_id: i64,
    generation: i64,
}

impl<'de> DeserializeSeed<'de> for LegacyCodexIncremental<'_> {
    type Value = Option<CodexIncrementalCache>;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_option(self)
    }
}

impl<'de> Visitor<'de> for LegacyCodexIncremental<'_> {
    type Value = Option<CodexIncrementalCache>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("optional legacy Codex resume metadata")
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer
            .deserialize_tuple(4, LegacyCodexFields(self))
            .map(Some)
    }
}

struct LegacyCodexFields<'a>(LegacyCodexIncremental<'a>);

impl<'de> Visitor<'de> for LegacyCodexFields<'_> {
    type Value = CodexIncrementalCache;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("legacy Codex resume fields")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let state = sequence
            .next_element_seed(LegacyCodexState(self.0))?
            .ok_or_else(|| serde::de::Error::custom("legacy Codex state is missing"))?;
        Ok(CodexIncrementalCache {
            state,
            consumed_offset: legacy_field(&mut sequence)?,
            ends_with_newline: legacy_field(&mut sequence)?,
            prefix_hash: legacy_field(&mut sequence)?,
        })
    }
}

struct LegacyCodexState<'a>(LegacyCodexIncremental<'a>);

impl<'de> DeserializeSeed<'de> for LegacyCodexState<'_> {
    type Value = CodexParseState;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_tuple(19, self)
    }
}

impl<'de> Visitor<'de> for LegacyCodexState<'_> {
    type Value = CodexParseState;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("legacy Codex parser state")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut state = CodexParseState {
            current_model: legacy_field(&mut sequence)?,
            current_turn_start_ms: legacy_field(&mut sequence)?,
            last_accepted_token_timestamp_ms: legacy_field(&mut sequence)?,
            previous_totals: legacy_field(&mut sequence)?,
            session_is_headless: legacy_field(&mut sequence)?,
            session_id_from_meta: legacy_field(&mut sequence)?,
            session_forked_from_id: legacy_field(&mut sequence)?,
            forked_child_session_id: legacy_field(&mut sequence)?,
            forked_child_replay_session_id: legacy_field(&mut sequence)?,
            session_provider: legacy_field(&mut sequence)?,
            session_agent: legacy_field(&mut sequence)?,
            session_workspace_key: legacy_field(&mut sequence)?,
            session_workspace_label: legacy_field(&mut sequence)?,
            forked_child_waiting_for_turn_context: legacy_field(&mut sequence)?,
            forked_child_inherited_baseline: legacy_field(&mut sequence)?,
            forked_child_inherited_reported_total: legacy_field(&mut sequence)?,
            pending_turn_start: legacy_field(&mut sequence)?,
            ..CodexParseState::default()
        };
        if sequence
            .next_element_seed(StreamSequence::<String, _>::new(|turn_id| {
                self.0
                    .connection
                    .prepare_cached("INSERT OR IGNORE INTO mc_codex_turn_ids VALUES(?1,?2,?3)")
                    .map_err(sql_error)?
                    .execute(params![self.0.source_id, self.0.generation, turn_id])
                    .map_err(sql_error)?;
                Ok(())
            }))?
            .is_none()
        {
            return Err(serde::de::Error::custom(
                "legacy Codex turn membership is missing",
            ));
        }
        state.forked_child_is_user_fork = legacy_field(&mut sequence)?;
        Ok(state)
    }
}

fn read_observed_history(
    connection: &Connection,
    identity: CacheIdentity,
    path: &Path,
    expected: &CachedSourceMetadata,
) -> Result<StoredMetadata, String> {
    let owns: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM mc_sources WHERE id=?1 AND namespace=?2 AND path=?3 AND (scope=?4 OR scope=?5))",
        params![expected.source_id,identity.namespace,encode_path(path)?,identity.scope.as_slice(),[0_u8;32].as_slice()],
        |row| row.get(0),
    ).map_err(sql_error)?;
    if !owns {
        return Err("source message cache observed history belongs to another source".to_string());
    }
    let stored = read_version(connection, expected.source_id, expected.generation)?;
    validate_stored_generation(connection, &stored)?;
    Ok(stored)
}

/// Caller owns the scope-publication transaction; a GC failure rolls back the
/// scope pointer and its source references together.
pub(crate) fn collect_unreferenced_versions_on(connection: &Connection) -> Result<(), String> {
    if connection.is_autocommit() {
        return Err("source message cache collection requires a scope transaction".to_string());
    }
    let scopes: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sq_scope_sources')",
        [],|row| row.get(0),
    ).map_err(sql_error)?;
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS mc_gc(source_id INTEGER,generation INTEGER,PRIMARY KEY(source_id,generation)) WITHOUT ROWID;
         DELETE FROM mc_gc;",
    ).map_err(sql_error)?;
    // Durable canonical history is pinned independent of source presence,
    // scope eviction, or cacheability: only a superseding newer generation of
    // the same source identity (or an explicit same-source replacement) may
    // retire it. GC drops purely stale intermediate generations and
    // version-less orphans; the newest generation per source identity always
    // survives, even when inactive or unreferenced by any live scope.
    let candidates = if scopes {
        "INSERT INTO mc_gc SELECT v.source_id,v.generation FROM mc_versions v JOIN mc_sources s ON s.id=v.source_id
         WHERE v.generation < (SELECT max(h.generation) FROM mc_versions h WHERE h.source_id=s.id)
         AND NOT EXISTS(SELECT 1 FROM mc_versions n WHERE n.source_id=v.source_id AND n.generation>v.generation
            AND (n.parser_version!=v.parser_version OR n.format_version!=v.format_version))
         AND NOT EXISTS(SELECT 1 FROM mc_migration_versions m WHERE m.source_id=v.source_id AND m.generation=v.generation)
         AND NOT EXISTS(SELECT 1 FROM sq_scope_sources q WHERE q.cache_source_id=v.source_id AND q.cache_generation=v.generation)"
    } else {
        "INSERT INTO mc_gc SELECT v.source_id,v.generation FROM mc_versions v JOIN mc_sources s ON s.id=v.source_id
         WHERE v.generation < (SELECT max(h.generation) FROM mc_versions h WHERE h.source_id=s.id)
         AND NOT EXISTS(SELECT 1 FROM mc_versions n WHERE n.source_id=v.source_id AND n.generation>v.generation
            AND (n.parser_version!=v.parser_version OR n.format_version!=v.format_version))
         AND NOT EXISTS(SELECT 1 FROM mc_migration_versions m WHERE m.source_id=v.source_id AND m.generation=v.generation)"
    };
    connection.execute(candidates, []).map_err(sql_error)?;
    // Empty source shells with no surviving version are removed below.
    connection.execute_batch(
        "DELETE FROM mc_rows WHERE EXISTS(SELECT 1 FROM mc_gc g WHERE g.source_id=mc_rows.source_id AND g.generation=mc_rows.generation);
         DELETE FROM mc_codex_turn_ids WHERE EXISTS(SELECT 1 FROM mc_gc g WHERE g.source_id=mc_codex_turn_ids.source_id AND g.generation=mc_codex_turn_ids.generation);
         DELETE FROM mc_versions WHERE EXISTS(SELECT 1 FROM mc_gc g WHERE g.source_id=mc_versions.source_id AND g.generation=mc_versions.generation);
         DELETE FROM mc_sources WHERE active=0 AND NOT EXISTS(SELECT 1 FROM mc_versions v WHERE v.source_id=mc_sources.id);
         DELETE FROM mc_gc;",
    ).map_err(sql_error)
}

#[cfg(test)]
#[path = "message_cache_disk_tests.rs"]
mod tests;

fn codex_ids_digest(
    connection: &Connection,
    source_id: i64,
    generation: i64,
) -> Result<(u64, Vec<u8>), String> {
    let mut hasher = Sha256::new();
    hasher.update(b"tokscale-codex-turn-membership-v1");
    let mut count = 0;
    let mut statement = connection.prepare("SELECT turn_id FROM mc_codex_turn_ids WHERE source_id=?1 AND generation=?2 ORDER BY turn_id").map_err(sql_error)?;
    let mut rows = statement
        .query(params![source_id, generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let id: String = row.get(0).map_err(sql_error)?;
        hasher.update((id.len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
        count += 1;
    }
    Ok((count, hasher.finalize().to_vec()))
}

/// Durable canonical history is independent of source presence: absent native
/// sources keep their last-good generation queryable. Only a superseding
/// newer generation of the same source identity retires a predecessor.
pub(crate) fn reconcile_missing_sources_on(
    connection: &Connection,
    previous_scope_generation: i64,
    current_scope_generation: i64,
) -> Result<(), String> {
    if connection.is_autocommit() {
        return Err("source message cache reconciliation requires a scope transaction".to_string());
    }
    if previous_scope_generation == 0 {
        return Ok(());
    }
    let same_scope: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sq_scope_generations p JOIN sq_scope_generations n ON n.scope_id=p.scope_id
         WHERE p.id=?1 AND n.id=?2 AND p.complete=1)",
        params![previous_scope_generation,current_scope_generation],|row| row.get(0),
    ).map_err(sql_error)?;
    if !same_scope {
        return Err("source message cache reconciliation scope does not match".to_string());
    }
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT c.id,c.generation,c.path FROM mc_sources c
         JOIN sq_scope_sources p ON p.cache_source_id=c.id
         WHERE p.scope_generation=?1 AND p.kind='cache' AND c.active=1
         AND NOT EXISTS(SELECT 1 FROM sq_scope_sources n
             WHERE n.scope_generation=?2 AND n.kind='cache' AND n.cache_source_id=c.id)
         ORDER BY c.id",
        )
        .map_err(sql_error)?;
    let mut rows = statement
        .query(params![previous_scope_generation, current_scope_generation])
        .map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let _source_id: i64 = row.get(0).map_err(sql_error)?;
        let _generation: i64 = row.get(1).map_err(sql_error)?;
        let path =
            decode::<CachedPath>(&row.get::<_, Vec<u8>>(2).map_err(sql_error)?)?.into_path_buf();
        match fs::metadata(path) {
            Ok(_) => {}
            // Absent sources keep their last-good generation: durable queries
            // must serve canonical facts after deletion or row removal.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("source message cache source discovery failed".to_string()),
        }
    }
    Ok(())
}

/// Validate every stored generation in an already-open durable handle:
/// payload hashes/codecs (frozen 3/4/5 via the shared decoder), row
/// completeness, fingerprint/metadata bounds, retained digests, and Codex
/// turn membership. No warm-parser admission gate may hide old readable
/// facts: every recorded version validates by its own recorded format.
pub(crate) fn validate_durable_store_on(connection: &Connection) -> Result<(), String> {
    let mut statement = connection
        .prepare("SELECT source_id,generation FROM mc_versions ORDER BY source_id,generation")
        .map_err(sql_error)?;
    let mut rows = statement.query([]).map_err(sql_error)?;
    let mut versions = Vec::new();
    while let Some(row) = rows.next().map_err(sql_error)? {
        versions.push((
            row.get::<_, i64>(0).map_err(sql_error)?,
            row.get::<_, i64>(1).map_err(sql_error)?,
        ));
    }
    drop(rows);
    drop(statement);
    for (source_id, generation) in versions {
        let stored = read_version(connection, source_id, generation)?;
        validate_stored_generation(connection, &stored)?;
    }
    Ok(())
}

fn validate_stored_generation(
    connection: &Connection,
    stored: &StoredMetadata,
) -> Result<(), String> {
    // Metadata alone is not completeness proof: missing rows must never be
    // normalized into a smaller, newly valid generation or recovery snapshot.
    visit_stored_rows(connection, stored, &mut |_: &UnifiedMessage, _, _| Ok(()))?;
    if retained_digest(connection, stored.id, stored.metadata.generation)? != stored.retained_digest
    {
        return Err("source message cache retained generation digest is invalid".to_string());
    }
    visit_codex_ids(connection, stored, &mut |_: &str| Ok(()))
}

fn verify_migration_transfer(
    connection: &Connection,
    path: &[u8],
    digest: &[u8],
    source_count: u64,
) -> Result<(), String> {
    let mut statement = connection.prepare(
        "SELECT source_id,generation FROM mc_migration_versions WHERE path=?1 AND digest=?2 ORDER BY source_id,generation",
    ).map_err(sql_error)?;
    let mut rows = statement.query(params![path, digest]).map_err(sql_error)?;
    let mut count = 0;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let stored = read_version(
            connection,
            row.get(0).map_err(sql_error)?,
            row.get(1).map_err(sql_error)?,
        )?;
        validate_stored_generation(connection, &stored)?;
        count += 1;
    }
    if count != source_count {
        return Err("source message cache migration evidence is incomplete".to_string());
    }
    Ok(())
}

fn retire_verified_shard(
    connection: &Connection,
    root: &Path,
    path: &Path,
    before: &SourceFingerprint,
    digest: &[u8; 32],
    source_count: u64,
) -> Result<(), String> {
    let encoded_path = encode_path(path)?;
    // A successful old transfer marker is not proof that its immutable rows
    // still exist. Validate before replacing the only last-good recovery copy.
    {
        let snapshot = connection.unchecked_transaction().map_err(sql_error)?;
        verify_migration_transfer(&snapshot, &encoded_path, digest, source_count)?;
        snapshot.commit().map_err(sql_error)?;
    }
    flush_recovery(connection, root)?;
    let transaction = connection.unchecked_transaction().map_err(sql_error)?;
    transaction
        .execute(
            "UPDATE mc_meta SET value=value WHERE key='schema_version'",
            [],
        )
        .map_err(sql_error)?;
    verify_migration_transfer(&transaction, &encoded_path, digest, source_count)?;
    if !fingerprint_is_current(path, before) || hash_prefix(path, before.size) != Some(*digest) {
        return Err("source message cache legacy shard changed during migration".to_string());
    }
    fs::remove_file(path)
        .map_err(|_| "source message cache migrated shard could not be retired".to_string())?;
    clear_migration_pin(&transaction, &encoded_path)?;
    transaction.commit().map_err(sql_error)
}

fn clear_migration_pin(connection: &Connection, path: &[u8]) -> Result<(), String> {
    connection
        .execute("DELETE FROM mc_migration_versions WHERE path=?1", [path])
        .map_err(sql_error)?;
    connection
        .execute("DELETE FROM mc_migrations WHERE path=?1", [path])
        .map_err(sql_error)?;
    Ok(())
}

fn release_missing_shard_pins(connection: &Connection, directory: &Path) -> Result<(), String> {
    // Recover a crash after unlink but before the marker-removal commit. Keep
    // evidence pins if either the shard or its transferred facts is uncertain.
    let transaction = connection.unchecked_transaction().map_err(sql_error)?;
    transaction
        .execute(
            "UPDATE mc_meta SET value=value WHERE key='schema_version'",
            [],
        )
        .map_err(sql_error)?;
    let mut statement = transaction
        .prepare("SELECT path,digest,source_count FROM mc_migrations ORDER BY path")
        .map_err(sql_error)?;
    let mut rows = statement.query([]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let encoded_path: Vec<u8> = row.get(0).map_err(sql_error)?;
        let path = decode::<CachedPath>(&encoded_path)?.into_path_buf();
        if path.parent() != Some(directory) {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let digest: Vec<u8> = row.get(1).map_err(sql_error)?;
                verify_migration_transfer(
                    &transaction,
                    &encoded_path,
                    &digest,
                    row.get(2).map_err(sql_error)?,
                )?;
                clear_migration_pin(&transaction, &encoded_path)?;
            }
            Err(_) => return Err("source message cache migrated shard is unreadable".to_string()),
        }
    }
    drop(rows);
    drop(statement);
    transaction.commit().map_err(sql_error)
}

pub(crate) fn validate_scope_metadata_on(
    connection: &Connection,
    scope_generation: i64,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT s.path,q.path,q.cache_source_id,q.cache_generation
         FROM sq_scope_sources q LEFT JOIN mc_sources s ON s.id=q.cache_source_id
         WHERE q.scope_generation=?1 AND q.kind IN ('cache','observed','observed-legacy')
         ORDER BY q.lane,q.source_order",
        )
        .map_err(sql_error)?;
    let mut rows = statement.query([scope_generation]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let source_path: Vec<u8> = row.get(0).map_err(sql_error)?;
        let captured_path: Vec<u8> = row.get(1).map_err(sql_error)?;
        if source_path != captured_path {
            return Err("pinned cache source descriptor does not match".into());
        }
        decode::<CachedPath>(&source_path)?;
        read_version(
            connection,
            row.get(2).map_err(sql_error)?,
            row.get(3).map_err(sql_error)?,
        )?;
    }
    Ok(())
}
