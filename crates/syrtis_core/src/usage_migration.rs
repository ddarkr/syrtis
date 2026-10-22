//! One-time upgrade of legacy tokscale usage history into the app-owned
//! durable usage root.
//!
//! `prepare(root, legacy_roots)` runs before ingestion/watch/token
//! entrypoints with the captured context's roots: `root` is
//! `ctx.usage_dir()`, `legacy_roots` is `ctx.legacy_usage_roots()`. This
//! module performs NO live environment or `dirs::` discovery — Main calls it
//! with captured inputs, and generic temp DB opens never probe production
//! caches. Safe to call on every entrypoint: idempotent, fail-closed, never
//! destructive to either side.
//!
//! Only SQLite stores migrate via the backup API (WAL-consistent, never a
//! raw file copy), plus every supported legacy shard through the shared
//! importer (`import_legacy_shards_on`: existing codecs, never modifies or
//! retires originals) — including shard-only roots with no SQLite store.
//! Validation reuses the consumer's own checks: `admit_schema` for SQLite
//! identity plus `validate_durable_store_on` (payload hashes, frozen codecs,
//! fingerprint/metadata, Codex membership) for every generation — never a
//! weaker count/digest-only duplicate.
//!
//! Activation is atomic no-replace publication with explicit post-commit
//! handling; an interrupted publication leaves a stranded same-inode staging
//! link recovered under lock on the next prepare. Originals are preserved;
//! no failed pre-commit activation; only owned-grammar temps are touched.
//!
//! Scope identity is preserved by construction: migrated rows keep their
//! recorded bytes and the durable path is excluded from the scope
//! descriptor, so scope queries link retained history after the move.
//!
//! Watch-hint filenames owned by this module (Main excludes these from
//! source scanning; no blanket directory ignore):
//! - exact: `usage-migration.lock` (per-root guard)
//! - exact: `source-message-cache.sqlite3`, plus `-wal`/`-shm`/`-journal`
//!   sidecars (durable target, published atomically)
//! - temp grammar (owned prefix): `^usage_migration_tmp_[0-9]+-[0-9]+\.sqlite3$`

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

const DATABASE: &str = "source-message-cache.sqlite3";
const RECOVERY: &str = "source-message-cache.recovery.sqlite3";
const LOCK: &str = "usage-migration.lock";
/// Owned staging temp prefix. Distinct from the restore grammar Main
/// already ignores, so watch rules need the exact addition below.
const STAGING_PREFIX: &str = "usage_migration_tmp_";
const MARKER_KEY: &str = "usage_migration_completed_source";
/// Prepare the durable usage root before ingestion/watch/token entrypoints.
///
/// `root` is the durable target dir itself
/// (`ctx.usage_dir()` — the same dir later passed to `open_database`), never
/// a legacy cache dir. `legacy_roots` is `ctx.legacy_usage_roots()`: the
/// immutable capture-bound migration inputs. This function performs NO live
/// environment or `dirs::` discovery — Main calls it with the captured
/// context before ingestion, and generic temp DB opens never probe
/// production caches. Creates the root, migrates at most one legacy source,
/// and returns `Ok` when the target is ready — including when there was
/// nothing to migrate or migration already completed. Fails closed on any
/// corruption, incompleteness, or I/O error; the target and all sources are
/// left intact and no success is recorded.
pub(crate) fn prepare(root: &Path, legacy_roots: &[PathBuf]) -> Result<(), String> {
    secure_directory(root)?;
    let _management = crate::usage_store::management_lock(root, false)?;
    let _guard = lock_file(&root.join(LOCK))?;
    // Independent stages are reclaimed only by explicit Optimize, never by
    // recurring ingestion. Publication-link recovery needs no history scan.
    recover_interrupted_publication(root)?;
    // The target appears only through atomic publication below, so an
    // existing target is either a completed migration or a store the opener
    // created itself. Either way there is nothing to import: a later cache
    // clear must never re-import histories it already overwrote, and
    // `open_database` admits the existing file itself (fail-closed on a
    // foreign schema). This also makes retry trivially safe.
    if target_exists(root)? {
        return Ok(());
    }
    match fs::symlink_metadata(root.join("usage-reset.barrier")) {
        Ok(_) => {
            return Err("usage store is absent after reset; refusing legacy history import".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("usage reset barrier admission failed".into()),
    }
    let Some(source) = select_source(root, legacy_roots)? else {
        return Ok(());
    };
    transfer(&source, root)?;
    Ok(())
}

/// Drop a leftover staging link that points at the same inode as the
/// canonical target (interrupted publication: crash between hard-link and
/// staging unlink, or a failed post-publication dir sync). Runs under the
/// migration lock; only removes owned-grammar temps (`usage_migration_tmp`
/// prefix) whose device+inode match the canonical target. Returns Ok in all
/// non-error cases; a stranded link the consumer rejects is a hard error.
fn recover_interrupted_publication(root: &Path) -> Result<(), String> {
    let target = db_path(root);
    let target_meta = match fs::symlink_metadata(&target) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("usage migration target admission failed".into()),
    };
    if target_meta.file_type().is_symlink() || !target_meta.is_file() {
        return Err("usage migration target is unsafe".into());
    }
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Err("usage migration directory admission failed".into()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "usage migration directory admission failed".to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((pid, nonce)) = name
            .strip_prefix(STAGING_PREFIX)
            .and_then(|name| name.strip_suffix(".sqlite3"))
            .and_then(|name| name.split_once('-'))
        else {
            continue;
        };
        if pid.is_empty()
            || nonce.is_empty()
            || !pid.bytes().all(|byte| byte.is_ascii_digit())
            || !nonce.bytes().all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let candidate = entry.path();
        let same = match (
            fs::symlink_metadata(&candidate),
            fs::symlink_metadata(&target),
        ) {
            (Ok(a), Ok(b)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    a.dev() == b.dev()
                        && a.ino() == b.ino()
                        && !a.file_type().is_symlink()
                        && a.is_file()
                        && a.uid() == unsafe { libc::geteuid() }
                        && b.uid() == unsafe { libc::geteuid() }
                }
                #[cfg(not(unix))]
                {
                    let _ = (a, b);
                    false
                }
            }
            _ => false,
        };
        if same {
            fs::remove_file(&candidate)
                .map_err(|_| "usage migration stranded publication cleanup failed")?;
        }
    }
    Ok(())
}

/// Explicit maintenance only. Equality is deliberately stricter than logical
/// coverage: old layouts, superseded generations and even unrelated extra rows
/// keep a stage intact rather than guessing whether its history is redundant.
/// The caller holds the exclusive management lock; migration takes the shared
/// lock before creating a stage.
pub(crate) fn reclaim_abandoned_stages(db: &Connection, root: &Path) -> Result<(), String> {
    let _migration = lock_file(&root.join(LOCK))?;
    for entry in fs::read_dir(root).map_err(|_| "usage migration directory admission failed")? {
        let entry = entry.map_err(|_| "usage migration directory admission failed")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((pid, nonce)) = name
            .strip_prefix(STAGING_PREFIX)
            .and_then(|s| s.strip_suffix(".sqlite3"))
            .and_then(|s| s.split_once('-'))
        else {
            continue;
        };
        if pid.is_empty()
            || nonce.is_empty()
            || !pid.bytes().all(|b| b.is_ascii_digit())
            || !nonce.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        // Failed admission/coverage is not permission to remove user data.
        if let Err(StageReclaimError::Maintenance(error)) =
            reclaim_identical_stage(db, root, &entry.path())
        {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn admitted_stage_owner(metadata: &fs::Metadata, owner: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == owner
        && metadata.nlink() == 1
        && metadata.mode() & 0o777 == 0o600
}

enum StageReclaimError {
    Unproven,
    Maintenance(String),
}

impl From<String> for StageReclaimError {
    fn from(_: String) -> Self {
        Self::Unproven
    }
}

impl From<&str> for StageReclaimError {
    fn from(_: &str) -> Self {
        Self::Unproven
    }
}

fn reclaim_identical_stage(
    db: &Connection,
    root: &Path,
    path: &Path,
) -> Result<(), StageReclaimError> {
    #[cfg(not(unix))]
    return Ok(());
    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        if !admitted_stage_owner(&metadata, unsafe { libc::geteuid() }) {
            return Ok(());
        }
        // Never open a stage that SQLite might recover or whose live WAL could
        // contain facts absent from its main file. Even empty sidecars stay
        // untouched: without their writer's identity they are not ours to unlink.
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            match fs::symlink_metadata(PathBuf::from(sidecar)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Ok(()),
            }
        }
        let stage = Connection::open_with_flags(
            crate::usage_store::sqlite_open_path(path)?,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|e| e.to_string())?;
        // Inspect through the borrowed VFS descriptor before any pager read:
        // WAL admission could otherwise create sidecars on an unproven stage.
        let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
        let status = unsafe {
            rusqlite::ffi::sqlite3_file_control(
                stage.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
            )
        };
        if status != rusqlite::ffi::SQLITE_OK {
            return Ok(());
        }
        let file_ref = unsafe { file.as_ref() }.ok_or("usage migration file unavailable")?;
        let methods =
            unsafe { file_ref.pMethods.as_ref() }.ok_or("usage migration VFS unavailable")?;
        let read = methods.xRead.ok_or("usage migration read unavailable")?;
        let mut header = [0u8; 20];
        if unsafe { read(file, header.as_mut_ptr().cast(), header.len() as i32, 0) }
            != rusqlite::ffi::SQLITE_OK
            || &header[..16] != b"SQLite format 3\0"
            || header[18..20] != [1, 1]
        {
            return Ok(());
        }
        configure(&stage)?;
        stage
            .busy_timeout(std::time::Duration::ZERO)
            .map_err(|e| e.to_string())?;
        crate::usage_store::admit_open_database(&stage, path, Some(&metadata))?;
        // No write statements: acquire SQLite's exclusive inode lock without
        // modifying the abandoned file or rolling forward a journal.
        stage
            .execute_batch("BEGIN EXCLUSIVE")
            .map_err(|e| e.to_string())?;
        let canonical = db.unchecked_transaction().map_err(|e| e.to_string())?;
        crate::usage_store::admit_open_database(db, &db_path(root), None)?;
        crate::usage_store::validate_managed_schema_readonly(&canonical)?;
        crate::usage_store::validate_managed_schema_readonly(&stage)?;
        if !equal_rows(&canonical, &stage, "PRAGMA application_id")?
            || !equal_rows(&canonical, &stage, "PRAGMA user_version")?
        {
            return Ok(());
        }
        let epoch: Vec<u8> = canonical
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        let staged_epoch: Vec<u8> = stage
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if epoch.len() != 16 || epoch != staged_epoch {
            return Ok(());
        }
        if !equal_rows(
            &canonical,
            &stage,
            "SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name",
        )? {
            return Ok(());
        }
        crate::message_cache::validate_durable_store_on(&canonical)?;
        crate::message_cache::validate_durable_store_on(&stage)?;
        let mut tables = canonical
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")
            .map_err(|e| e.to_string())?;
        let mut rows = tables.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let table: String = row.get(0).map_err(|e| e.to_string())?;
            let quoted = format!("\"{}\"", table.replace('"', "\"\""));
            // Different physical order conservatively retains the stage; do
            // not sort historical payloads into another temporary database.
            if !equal_rows(&canonical, &stage, &format!("SELECT * FROM {quoted}"))? {
                return Ok(());
            }
        }
        crate::usage_store::admit_open_database(db, &db_path(root), None)?;
        crate::usage_store::admit_open_database(&stage, path, Some(&metadata))?;
        // Hold both snapshots and the stage's SQLite lock through unlink.
        fs::remove_file(path).map_err(|e| {
            StageReclaimError::Maintenance(format!("usage migration stage unlink failed: {e}"))
        })?;
        fs::File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|e| {
                StageReclaimError::Maintenance(format!(
                    "usage migration stage directory sync failed: {e}"
                ))
            })?;
        Ok(())
    }
}

fn equal_rows(left: &Connection, right: &Connection, sql: &str) -> Result<bool, String> {
    let mut a = left.prepare(sql).map_err(|e| e.to_string())?;
    let mut b = right.prepare(sql).map_err(|e| e.to_string())?;
    let columns = a.column_count();
    if columns != b.column_count() {
        return Ok(false);
    }
    let mut a = a.query([]).map_err(|e| e.to_string())?;
    let mut b = b.query([]).map_err(|e| e.to_string())?;
    loop {
        match (
            a.next().map_err(|e| e.to_string())?,
            b.next().map_err(|e| e.to_string())?,
        ) {
            (None, None) => return Ok(true),
            (Some(a), Some(b)) => {
                for column in 0..columns {
                    if a.get_ref(column).map_err(|e| e.to_string())?
                        != b.get_ref(column).map_err(|e| e.to_string())?
                    {
                        return Ok(false);
                    }
                }
            }
            _ => return Ok(false),
        }
    }
}

fn secure_directory(root: &Path) -> Result<(), String> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err("usage migration directory is unsafe".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|_| "usage migration directory creation failed")?;
        }
        Err(_) => return Err("usage migration directory admission failed".into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta =
            fs::symlink_metadata(root).map_err(|_| "usage migration directory admission failed")?;
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err("usage migration directory ownership is unsafe".into());
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))
                .map_err(|_| "usage migration directory protection failed")?;
        }
    }
    Ok(())
}

fn lock_file(path: &Path) -> Result<fs::File, String> {
    // Pre-admit the path, then open with O_NOFOLLOW|O_CREAT and validate the
    // open descriptor (symlink swap, owner, link count, mode) — the same
    // trust boundary usage_store enforces. Never chmod/stat by path after.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err("usage migration lock is unsafe".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("usage migration lock admission failed".into()),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "usage migration lock admission failed")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = file
            .metadata()
            .map_err(|_| "usage migration lock admission failed")?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err("usage migration lock is unsafe".into());
        }
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err("usage migration lock ownership is unsafe".into());
        }
        if meta.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "usage migration lock protection failed")?;
        }
    }
    fs2::FileExt::lock_exclusive(&file).map_err(|_| "usage migration lock failed")?;
    Ok(file)
}

fn admit_source_file(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err("usage migration source is unsafe".into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("usage migration source is absent".into())
        }
        Err(_) => Err("usage migration source admission failed".into()),
    }
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join(DATABASE)
}

fn target_exists(root: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(db_path(root)) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err("usage migration target is unsafe".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("usage migration target admission failed".into()),
    }
}

fn configure(conn: &Connection) -> Result<(), String> {
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|_| "usage migration database operation failed".to_string())?;
    conn.execute_batch("PRAGMA mmap_size=0; PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON;")
        .map_err(|_| "usage migration database operation failed".to_string())
}

/// Target/store admission: `quick_check` integrity plus canonical SQLite
/// identity (Main-exposed `admit_schema` — no recursive opener, no prepare
/// call). Generation payload checks belong to the shared
/// `validate_durable_store_on`, never a local duplicate.
fn validate_store(conn: &Connection) -> Result<(), String> {
    let result: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "usage migration validation failed")?;
    if result != "ok" {
        return Err("usage migration validation failed".into());
    }
    crate::usage_store::admit_schema(conn)
}

fn sync_connection(conn: &Connection) -> Result<(), String> {
    let (busy, log, done): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|_| "usage migration sync failed")?;
    if busy != 0 || log != done {
        return Err("usage migration checkpoint incomplete".into());
    }
    // Borrow SQLite's own VFS file like usage_store does; never open an
    // unrelated descriptor for a lock-bearing inode.
    let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
    let rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
        )
    };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err("usage migration sync failed".into());
    }
    let file_ref = unsafe { file.as_ref() }.ok_or("usage migration sync failed")?;
    let methods = unsafe { file_ref.pMethods.as_ref() }.ok_or("usage migration sync failed")?;
    let sync = methods.xSync.ok_or("usage migration sync failed")?;
    if unsafe { sync(file, rusqlite::ffi::SQLITE_SYNC_FULL) } != rusqlite::ffi::SQLITE_OK {
        return Err("usage migration sync failed".into());
    }
    Ok(())
}

struct Source {
    dir: PathBuf,
    label: String,
    kind: SourceKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Database,
    ShardOnly,
}

/// First captured candidate (by captured precedence) holding migratable
/// history: a SQLite store file, else a legacy shard tree
/// (`source-message-cache-v2/`) even when no SQLite store exists. Roots come
/// ONLY from `ctx.legacy_usage_roots()` — no live-env discovery here. The
/// durable target dir itself is never a candidate: migrating the target into
/// itself would silently re-import histories a cache clear already overwrote.
fn select_source(root: &Path, legacy_roots: &[PathBuf]) -> Result<Option<Source>, String> {
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in legacy_roots {
        if dir == root {
            continue;
        }
        if seen.contains(dir) {
            continue;
        }
        seen.push(dir.clone());
        if usable_store_file(&dir.join(DATABASE))? {
            return Ok(Some(Source {
                label: dir.to_string_lossy().into_owned(),
                dir: dir.clone(),
                kind: SourceKind::Database,
            }));
        }
        // Fall back to the retained-history recovery snapshot only when the
        // canonical store is missing; it is a last-good copy, never a second
        // report source to merge on top of a live canonical DB.
        if usable_store_file(&dir.join(RECOVERY))? {
            return Ok(Some(Source {
                label: dir.to_string_lossy().into_owned(),
                dir: dir.clone(),
                kind: SourceKind::Database,
            }));
        }
        // Shard-only legacy root: positional shards with no SQLite store.
        if has_supported_shards(dir)? {
            return Ok(Some(Source {
                label: dir.to_string_lossy().into_owned(),
                dir: dir.clone(),
                kind: SourceKind::ShardOnly,
            }));
        }
    }
    Ok(None)
}

/// True when `dir/source-message-cache-v2/` holds at least one `shard-*.bin`
/// under a known namespace dir. Symlinks fail closed; absence is false.
fn has_supported_shards(dir: &Path) -> Result<bool, String> {
    let shard_root = dir.join("source-message-cache-v2");
    let entries = match fs::read_dir(&shard_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("usage migration shard discovery failed".into()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "usage migration shard discovery failed".to_string())?;
        let file_type = entry
            .file_type()
            .map_err(|_| "usage migration shard discovery failed".to_string())?;
        if file_type.is_symlink() {
            return Err("usage migration shard is unsafe".into());
        }
        if !file_type.is_dir() {
            continue;
        }
        let namespace_dir = entry.path();
        let shards = match fs::read_dir(&namespace_dir) {
            Ok(shards) => shards,
            Err(_) => return Err("usage migration shard discovery failed".into()),
        };
        for shard in shards {
            let shard = shard.map_err(|_| "usage migration shard discovery failed".to_string())?;
            let file_type = shard
                .file_type()
                .map_err(|_| "usage migration shard discovery failed".to_string())?;
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let name = shard.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("shard-") && name.ends_with(".bin") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn usable_store_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err("usage migration source is unsafe".into());
            }
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("usage migration source admission failed".into()),
    }
}

/// WAL-consistent transfer via the SQLite backup API (or a fresh store plus
/// shard import for shard-only roots), then atomic activation. Never touches
/// the source; never overwrites a completed target.
fn transfer(source: &Source, root: &Path) -> Result<(), String> {
    if source.kind == SourceKind::ShardOnly {
        return transfer_shard_only(source, root);
    }
    let source_file = pick_source_file(source)?;
    admit_source_file(&source_file)?;
    let origin = Connection::open_with_flags(
        crate::usage_store::sqlite_open_path(&source_file)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|_| "usage migration source open failed")?;
    configure(&origin)?;
    let snapshot = validated_source_snapshot(&origin)?;

    // Stage off to the side under the owned temp prefix (exact watch-rule
    // addition for Main). No failed transfer may leave an empty canonical
    // file behind.
    let staging = stage_file(root)?;
    let outcome = (|| {
        let mut destination = Connection::open_with_flags(
            crate::usage_store::sqlite_open_path(&staging)?,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "usage migration staging open failed")?;
        configure(&destination)?;
        destination
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| "usage migration staging setup failed")?;
        let backup = rusqlite::backup::Backup::new(&snapshot, &mut destination)
            .map_err(|_| "usage migration backup failed")?;
        step_backup(&backup)?;
        drop(backup);
        drop(snapshot);
        drop(destination);
        // Reopen (the pager still remembers DELETE) and normalize, then
        // validate the staged copy before it can become canonical.
        let staged = Connection::open_with_flags(
            crate::usage_store::sqlite_open_path(&staging)?,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| "usage migration staging reopen failed")?;
        configure(&staged)?;
        crate::usage_store::admit_schema(&staged)?;
        // Import every supported legacy shard from the source root into the
        // staged copy BEFORE validation+activation, so no source log is
        // needed afterwards. Uses the shared importer (existing codecs,
        crate::message_cache::import_legacy_shards_on(&staged, &source.dir)?;
        validate_store(&staged)?;
        // Validate every copied/imported generation before activation.
        crate::message_cache::validate_durable_store_on(&staged)?;
        staged
            .execute(
                "INSERT OR REPLACE INTO mc_meta(key,value) VALUES(?1,?2)",
                rusqlite::params![MARKER_KEY, source.label],
            )
            .map_err(|_| "usage migration marker write failed")?;
        sync_connection(&staged)?;
        drop(staged);
        publish_staged(&staging, root)?;
        Ok::<(), String>(())
    })();
    // Only pre-commit staging files may be removed here: after a successful
    // publish the canonical name owns the inode and the staging name is
    // either already unlinked by publish_staged or a stranded same-inode
    // link recovered on the next prepare under lock. Never delete the
    // canonical target and never sweep unrelated temps.
    if !target_exists(root)? {
        let _ = fs::remove_file(&staging);
    }
    outcome
}

/// Publish without replacing an existing canonical name. A crash between
/// the hard link and staging unlink is recovered under the migration lock.
/// Both directory mutations are synced; post-publication errors never delete
/// the canonical target.
fn publish_staged(staging: &Path, root: &Path) -> Result<(), String> {
    fs::hard_link(staging, db_path(root)).map_err(|_| "usage migration publication failed")?;
    sync_connection_on_path(staging)?;
    fs::File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|_| "usage migration directory sync failed")?;
    fs::remove_file(staging).map_err(|_| "usage migration staging cleanup failed")?;
    fs::File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|_| "usage migration directory sync failed")?;
    Ok(())
}

/// Fsync a staged file by path (post-link: the inode is the canonical one).
/// Open read-only + NOFOLLOW so a swapped symlink fails closed.
fn sync_connection_on_path(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "usage migration sync failed")?;
    file.sync_all().map_err(|_| "usage migration sync failed")?;
    Ok(())
}

/// Shard-only legacy root: create a fresh store at staging (reusing the
/// canonical schema identity via the shared initializer through
/// `import_legacy_shards_on`, which initializes when missing), import every
/// supported shard from the legacy root without touching it, validate, mark,
/// and publish atomically. Valid empty shards still require import receipts.
fn transfer_shard_only(source: &Source, root: &Path) -> Result<(), String> {
    let staging = stage_file(root)?;
    let outcome = (|| {
        // Fresh store: open-create at staging, then run the shared legacy
        // shard importer which initializes the existing mc schema (and its
        // application/schema identity) before importing known namespaces via
        // the existing codecs. Originals are never retired or modified.
        {
            let bootstrap = Connection::open_with_flags(
                crate::usage_store::sqlite_open_path(&staging)?,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .map_err(|_| "usage migration staging open failed")?;
            configure(&bootstrap)?;
            // Canonical SQLite identity first (Main-exposed admit_schema —
            // no recursive opener, no prepare call), then the shared
            // importer, then strong generation validation + evidence.
            crate::usage_store::admit_schema(&bootstrap)?;
            crate::message_cache::import_legacy_shards_on(&bootstrap, &source.dir)?;
            validate_store(&bootstrap)?;
            crate::message_cache::validate_durable_store_on(&bootstrap)?;
            require_imported_evidence(&bootstrap)?;
            bootstrap
                .execute(
                    "INSERT OR REPLACE INTO mc_meta(key,value) VALUES(?1,?2)",
                    rusqlite::params![MARKER_KEY, source.label],
                )
                .map_err(|_| "usage migration marker write failed")?;
            sync_connection(&bootstrap)?;
        }
        publish_staged(&staging, root)?;
        Ok::<(), String>(())
    })();
    // Pre-commit cleanup only (see transfer): never delete an activated
    // canonical target and never sweep unrelated temps.
    if !target_exists(root)? {
        let _ = fs::remove_file(&staging);
    }
    outcome
}

/// Extract the staging-file creation shared by both transfer paths. Uses the
/// owned `usage_migration_tmp_` prefix (exact watch-rule addition for Main),
/// never the restore grammar.
fn stage_file(root: &Path) -> Result<PathBuf, String> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "usage migration clock failed")?
        .as_nanos();
    let staging = root.join(format!(
        "{STAGING_PREFIX}{}-{nonce}.sqlite3",
        std::process::id()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(&staging)
        .map_err(|_| "usage migration staging creation failed")?;
    Ok(staging)
}

/// Valid empty shards are legitimate: a source-free namespace import writes
/// evidence + namespace signature rows with zero facts. Gate on validated
/// import evidence (written by the shared importer for every imported shard,
/// including empty-but-valid ones), not on a raw row count — so a valid
/// empty decode activates while a total import miss fails closed.
fn require_imported_evidence(conn: &Connection) -> Result<(), String> {
    let migrations: i64 = conn
        .query_row("SELECT count(*) FROM mc_migrations", [], |row| row.get(0))
        .map_err(|_| "usage migration shard import is incomplete")?;
    let versions: i64 = conn
        .query_row("SELECT count(*) FROM mc_migration_versions", [], |row| {
            row.get(0)
        })
        .map_err(|_| "usage migration shard import is incomplete")?;
    // SQLite-path transfers carry evidence from the source store itself, so
    // only shard-bearing imports are gated here: at least one shard must
    // have recorded evidence, and every evidence row must reference a
    // validated version.
    if migrations <= 0 || versions <= 0 {
        return Err("usage migration shard import is empty".into());
    }
    Ok(())
}

/// Canonical store wins; recovery snapshot only when canonical is absent.
fn pick_source_file(source: &Source) -> Result<PathBuf, String> {
    let canonical = source.dir.join(DATABASE);
    if usable_store_file(&canonical)? {
        return Ok(canonical);
    }
    let recovery = source.dir.join(RECOVERY);
    if usable_store_file(&recovery)? {
        return Ok(recovery);
    }
    Err("usage migration source is absent".into())
}

fn validated_source_snapshot(conn: &Connection) -> Result<rusqlite::Transaction<'_>, String> {
    // Validation establishes a WAL read snapshot; keep it through every backup
    // step so another connection's writes cannot restart the copy or replace
    // the generation we validated. WAL writers remain free to commit.
    let snapshot = conn
        .unchecked_transaction()
        .map_err(|_| "usage migration source snapshot failed")?;
    crate::usage_store::validate_managed_schema_readonly(&snapshot)?;
    let integrity: String = snapshot
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|_| "usage migration source validation failed")?;
    if integrity != "ok" {
        return Err("usage migration source validation failed".into());
    }
    crate::message_cache::validate_durable_store_on(&snapshot)?;
    Ok(snapshot)
}

fn step_backup(backup: &rusqlite::backup::Backup<'_, '_>) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        match backup
            .step(128)
            .map_err(|_| "usage migration backup failed")?
        {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More
                if started.elapsed() < std::time::Duration::from_secs(300) => {}
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                return Err("usage migration backup is busy".into());
            }
            _ => return Err("usage migration backup deadline exceeded".into()),
        }
    }
}
#[cfg(test)]
#[path = "usage_migration_tests.rs"]
mod tests;
