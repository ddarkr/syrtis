//! Storage-only management: never discovers or parses source logs.
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const DATABASE: &str = "source-message-cache.sqlite3";
const RECOVERY: &str = "source-message-cache.recovery.sqlite3";
const PRICING: [&str; 2] = ["pricing-litellm.json", "pricing-openrouter.json"];

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DiskUsageAction {
    Inspect,
    ClearCache,
    Optimize,
    ResetUsage,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageClient {
    pub client: String,
    pub record_count: u64,
    pub first_timestamp_ms: Option<i64>,
    pub last_timestamp_ms: Option<i64>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskUsageReport {
    pub usage_directory: String,
    pub usage_bytes: u64,
    pub recovery_bytes: u64,
    pub cache_bytes: u64,
    pub other_bytes: u64,
    pub clients: Vec<DiskUsageClient>,
    pub measured_at_ms: i64,
}
fn error(e: impl std::fmt::Display) -> String {
    format!("Disk usage: {e}")
}
fn table(db: &Connection, name: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )
    .map_err(error)
}
fn regular(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => Ok(Some(m)),
        Ok(_) => Err("Disk usage: unsafe managed file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(error(e)),
    }
}
fn allocated(m: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        m.len()
    }
}
fn bytes(path: &Path) -> Result<u64, String> {
    let m = fs::symlink_metadata(path).map_err(error)?;
    // Count the link itself, never its target. Unknown directories are measured,
    // but are not candidates for deletion.
    if !m.is_dir() || m.file_type().is_symlink() {
        return Ok(allocated(&m));
    }
    #[cfg(unix)]
    {
        use std::os::unix::{fs::OpenOptionsExt, io::IntoRawFd};
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(error)?;
        directory_bytes(file.into_raw_fd())
    }
    #[cfg(not(unix))]
    {
        let mut n = allocated(&m);
        for child in fs::read_dir(path).map_err(error)? {
            n = n.saturating_add(bytes(&child.map_err(error)?.path())?);
        }
        Ok(n)
    }
}
#[cfg(unix)]
fn directory_bytes(fd: std::os::fd::RawFd) -> Result<u64, String> {
    // fdopendir owns fd; all descendants use openat/fstatat, never path-based
    // recursion that could follow a directory swapped for a symlink.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(error(std::io::Error::last_os_error()));
    }
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let owned = Directory(dir);
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let mut n = (unsafe { stat.assume_init() }.st_blocks as u64).saturating_mul(512);
    loop {
        #[cfg(target_os = "macos")]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(target_os = "linux")]
        unsafe {
            *libc::__errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(owned.0) };
        if entry.is_null() {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error().unwrap_or(0) != 0 {
                return Err(error(e));
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                fd,
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(error(std::io::Error::last_os_error()));
        }
        let stat = unsafe { stat.assume_init() };
        let size = if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = unsafe {
                libc::openat(
                    fd,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if child < 0 {
                return Err(error(std::io::Error::last_os_error()));
            }
            directory_bytes(child)?
        } else {
            (stat.st_blocks as u64).saturating_mul(512)
        };
        n = n.saturating_add(size);
    }
    Ok(n)
}
fn root_exists(root: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(root) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(true),
        Ok(_) => Err("Disk usage: unsafe usage directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(error(e)),
    }
}
fn counts(db: &Connection) -> Result<Vec<DiskUsageClient>, String> {
    let mut selects = Vec::new();
    if table(db, "mc_sources")? {
        crate::message_cache::install_logical_views_on(db)?;
        selects.push("SELECT r.client,r.timestamp FROM mc_logical_rows r JOIN mc_sources s ON s.id=r.source_id AND s.generation=r.generation");
    }
    if table(db, "oc_sources")? {
        selects.push("SELECT 'opencode' AS client,m.timestamp FROM oc_messages m JOIN oc_sources s ON s.generation=m.source");
    }
    if selects.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!("SELECT client,count(*),min(timestamp),max(timestamp) FROM ({}) GROUP BY client ORDER BY client", selects.join(" UNION ALL "));
    let mut statement = db.prepare(&sql).map_err(error)?;
    let rows = statement
        .query_map([], |r| {
            Ok(DiskUsageClient {
                client: r.get(0)?,
                record_count: r.get(1)?,
                first_timestamp_ms: r.get(2)?,
                last_timestamp_ms: r.get(3)?,
            })
        })
        .map_err(error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(error)
}
/// Shared across processes for the whole ingestion operation. Reset takes the
/// exclusive side before changing facts, so old producers cannot republish.
pub(crate) fn ingestion_guard(root: &Path) -> Result<fs::File, String> {
    crate::usage_store::management_lock(root, false)
}

pub fn disk_usage(
    context: &crate::ResolvedLocalSourceContext,
    action: DiskUsageAction,
) -> Result<DiskUsageReport, String> {
    disk_usage_paths(
        context.usage_dir(),
        &context.pricing_config_dir().join("cache"),
        action,
    )
}

fn disk_usage_paths(
    root: &Path,
    cache: &Path,
    action: DiskUsageAction,
) -> Result<DiskUsageReport, String> {
    let exists = root_exists(root)?;
    let _guard = if action == DiskUsageAction::Inspect
        || (!exists && action != DiskUsageAction::ResetUsage)
    {
        None
    } else {
        Some(crate::usage_store::management_lock(root, true)?)
    };
    let present = exists && regular(&root.join(DATABASE))?.is_some();
    if action == DiskUsageAction::ClearCache && root_exists(cache)? {
        for name in PRICING {
            let p = cache.join(name);
            if regular(&p)?.is_some() {
                fs::remove_file(p).map_err(error)?;
            }
        }
    }
    if action == DiskUsageAction::ResetUsage {
        // A canonical empty store is an intentional migration barrier even when
        // no usage database existed before reset. Never unlink SQLite handles.
        let db = crate::usage_store::open_database(root)?;
        reset(&db, root)?;
    } else if action == DiskUsageAction::Optimize && present {
        let db = crate::usage_store::open_database(root)?;
        crate::usage_migration::reclaim_abandoned_stages(&db, root)?;
        compact(&db, root, fs2::available_space(root).map_err(error)?)?;
    }
    let mut report = DiskUsageReport {
        usage_directory: root.to_string_lossy().into_owned(),
        usage_bytes: 0,
        recovery_bytes: 0,
        cache_bytes: 0,
        other_bytes: 0,
        clients: Vec::new(),
        measured_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    if root_exists(root)? {
        report.other_bytes = allocated(&fs::symlink_metadata(root).map_err(error)?);
        if regular(&root.join(DATABASE))?.is_some() {
            let db = crate::usage_store::open_existing_database(root)?;
            db.execute_batch("BEGIN").map_err(error)?;
            report.clients = counts(&db)?;
        }
        for child in fs::read_dir(root).map_err(error)? {
            let child = child.map_err(error)?;
            let name = child.file_name();
            let name = name.to_string_lossy();
            let n = bytes(&child.path())?;
            let category = if managed(&name, DATABASE) {
                &mut report.usage_bytes
            } else if managed(&name, RECOVERY) {
                &mut report.recovery_bytes
            } else {
                &mut report.other_bytes
            };
            *category = category.saturating_add(n);
        }
    }
    if root_exists(cache)? {
        for name in PRICING {
            if let Some(m) = regular(&cache.join(name))? {
                report.cache_bytes = report.cache_bytes.saturating_add(allocated(&m));
            }
        }
    }
    Ok(report)
}
fn managed(name: &str, base: &str) -> bool {
    name == base
        || ["-wal", "-shm", "-journal"]
            .iter()
            .any(|s| name.strip_prefix(base) == Some(*s))
}

fn checkpoint(db: &Connection) -> Result<(), String> {
    let (busy, log, done): (i64, i64, i64) = db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(error)?;
    if busy != 0 || log != done {
        return Err(format!("Disk usage: checkpoint incomplete (busy={busy}, log={log}, checkpointed={done}); maintenance not completed"));
    }
    Ok(())
}

pub(crate) fn compact(db: &Connection, _root: &Path, available: u64) -> Result<(), String> {
    let pages: u64 = db
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .map_err(error)?;
    let page_size: u64 = db
        .pragma_query_value(None, "page_size", |r| r.get(0))
        .map_err(error)?;
    // VACUUM's temporary copy and rewritten WAL may coexist. Existing
    // recovery files are left untouched and require no additional space.
    let required = pages
        .saturating_mul(page_size)
        .saturating_mul(2)
        .saturating_add(16 * 1024 * 1024);
    if available < required {
        return Err(format!("Disk usage: insufficient free space for compaction (need {required}, available {available}); maintenance not completed"));
    }
    checkpoint(db)?;
    crate::usage_store::compact_opencode_candidates(db)
        .map_err(|e| format!("{e}; compaction maintenance not completed"))?;
    crate::store_query::compact_decision_metadata(db)
        .map_err(|e| format!("{e}; compaction maintenance not completed"))?;
    db.execute_batch("PRAGMA optimize; VACUUM;")
        .map_err(|e| format!("{}; compaction maintenance not completed", error(e)))?;
    checkpoint(db).map_err(|e| format!("{e}; database compacted but checkpoint incomplete"))
}
fn reset(db: &Connection, root: &Path) -> Result<(), String> {
    // Persist a fail-closed recovery barrier before committing deletion so
    // offline recovery cannot revive an existing pre-reset snapshot.
    let epoch: Vec<u8> = db
        .query_row("SELECT randomblob(16)", [], |r| r.get(0))
        .map_err(error)?;
    crate::usage_store::write_reset_barrier(root, &epoch)?;
    db.pragma_update(None, "secure_delete", true)
        .map_err(error)?;
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(error)?;
    // Known tables only: do not erase future unrelated configuration tables.
    for name in [
        "sq_decisions",
        "sq_indexes",
        "sq_scope_sources",
        "sq_scope_source_current",
        "sq_scope_source_pins",
        "sq_scope_generations",
        "sq_scopes",
        "sq_durable_origins",
        "mc_migration_versions",
        "mc_migrations",
        "mc_generation_storage",
        "mc_codex_turn_ids",
        "mc_rows",
        "mc_versions",
        "mc_sources",
        "mc_namespaces",
        "oc_aliases",
        "oc_messages",
        "oc_rows",
        "oc_sources",
        "oc_generations",
        "report_checkpoints",
    ] {
        if table(&tx, name)? {
            tx.execute_batch(&format!("DELETE FROM {name}"))
                .map_err(error)?;
        }
    }
    if table(&tx, "sq_meta")? {
        tx.execute(
            "UPDATE sq_meta SET value=randomblob(16) WHERE key='store_epoch'",
            [],
        )
        .map_err(error)?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS du_reset(epoch BLOB NOT NULL); DELETE FROM du_reset;",
    )
    .map_err(error)?;
    tx.execute("INSERT INTO du_reset VALUES(?1)", [&epoch])
        .map_err(error)?;
    tx.commit().map_err(error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compaction_preserves_epoch_and_facts_without_changing_existing_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("usage");
        drop(crate::store_query::QueryState::open(&root).unwrap());
        let db = crate::usage_store::open_database(&root).unwrap();
        db.execute_batch(
            "UPDATE sq_meta SET value=x'010203' WHERE key='store_epoch';
            CREATE TABLE retained_fact(value BLOB);
            INSERT INTO retained_fact VALUES(x'1234');
            CREATE TABLE disposable_padding(value BLOB);
            INSERT INTO disposable_padding VALUES(zeroblob(4194304));",
        )
        .unwrap();
        checkpoint(&db).unwrap();
        crate::usage_store::refresh_retained_recovery(&db, &root).unwrap();
        db.execute_batch("DROP TABLE disposable_padding").unwrap();
        let before = fs::metadata(root.join(DATABASE)).unwrap().len();
        let recovery_bytes = fs::read(root.join(RECOVERY)).unwrap();
        assert!(compact(&db, &root, 0)
            .unwrap_err()
            .contains("insufficient free space"));
        assert_eq!(fs::read(root.join(RECOVERY)).unwrap(), recovery_bytes);
        let pages: u64 = db
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        let page_size: u64 = db
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .unwrap();
        let required = pages * page_size * 2 + 16 * 1024 * 1024;
        assert!(compact(&db, &root, required - 1).is_err());
        compact(&db, &root, required).unwrap();
        assert!(fs::metadata(root.join(DATABASE)).unwrap().len() < before);
        assert_eq!(fs::read(root.join(RECOVERY)).unwrap(), recovery_bytes);
        let recovery = Connection::open(root.join(RECOVERY)).unwrap();
        for connection in [&db, &recovery] {
            assert_eq!(
                connection
                    .query_row("SELECT value FROM retained_fact", [], |r| r
                        .get::<_, Vec<u8>>(0))
                    .unwrap(),
                [0x12, 0x34]
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT value FROM sq_meta WHERE key='store_epoch'",
                        [],
                        |r| r.get::<_, Vec<u8>>(0)
                    )
                    .unwrap(),
                [1, 2, 3]
            );
        }
    }

    #[test]
    fn checkpoint_reports_busy_reader_without_unlinking_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("usage");
        let db = crate::usage_store::open_database(&root).unwrap();
        db.execute_batch("CREATE TABLE fact(value); INSERT INTO fact VALUES(1)")
            .unwrap();
        checkpoint(&db).unwrap();
        let reader = Connection::open(root.join(DATABASE)).unwrap();
        reader.execute_batch("BEGIN; SELECT * FROM fact;").unwrap();
        db.execute_batch("INSERT INTO fact VALUES(2)").unwrap();
        db.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(checkpoint(&db)
            .unwrap_err()
            .contains("checkpoint incomplete"));
        assert!(root.join(format!("{DATABASE}-wal")).exists());
        reader.execute_batch("ROLLBACK").unwrap();
        checkpoint(&db).unwrap();
    }

    #[test]
    fn allocated_bytes_and_symlink_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a");
        fs::write(&p, b"owned").unwrap();
        assert_eq!(bytes(&p).unwrap(), allocated(&fs::metadata(&p).unwrap()));
        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&p, &link).unwrap();
            assert!(regular(&link).is_err());
            assert_eq!(
                bytes(&link).unwrap(),
                allocated(&fs::symlink_metadata(link).unwrap())
            );
        }
        assert!(!root_exists(&tmp.path().join("absent")).unwrap());
        assert!(!tmp.path().join("absent").exists());
    }

    #[test]
    fn inspect_and_optimize_preserve_stage_when_canonical_is_unavailable() {
        for corrupt in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("usage");
            let cache = tmp.path().join("cache");
            drop(crate::usage_store::open_database(&root).unwrap());
            let canonical = root.join(DATABASE);
            let stage = root.join("usage_migration_tmp_123-456.sqlite3");
            fs::rename(&canonical, &stage).unwrap();
            let original = fs::read(&stage).unwrap();
            if corrupt {
                fs::write(&canonical, b"corrupt").unwrap();
            }
            let before = fs::read(&canonical).ok();
            for action in [DiskUsageAction::Inspect, DiskUsageAction::Optimize] {
                let result = disk_usage_paths(&root, &cache, action);
                assert_eq!(result.is_err(), corrupt);
                assert_eq!(fs::read(&stage).unwrap(), original);
                assert_eq!(fs::read(&canonical).ok(), before);
            }
        }
    }

    #[test]
    fn management_preserves_then_resets_facts_without_creating_on_inspect() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("usage");
        let cache = tmp.path().join("cache");
        let empty = disk_usage_paths(&root, &cache, DiskUsageAction::Inspect).unwrap();
        assert!(empty.clients.is_empty());
        assert!(!root.exists());
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join(PRICING[0]), b"disposable").unwrap();
        fs::write(cache.join("source-message-cache.bin"), b"only-copy").unwrap();
        let db = crate::usage_store::open_database(&root).unwrap();
        let mut sources = Vec::new();
        for timestamp in [100, 200] {
            let path = tmp.path().join(format!("message-{timestamp}.json"));
            let fact = serde_json::json!({
                "role": "assistant", "id": format!("message-{timestamp}"),
                "sessionID": "session", "modelID": "gpt-4o", "providerID": "openai",
                "tokens": {"input": 3, "output": 2, "cache": {"read": 0, "write": 0}},
                "time": {"created": timestamp}
            });
            fs::write(&path, fact.to_string()).unwrap();
            sources.push(path);
        }
        crate::usage_store::synchronize_opencode_sources(
            &[],
            &sources,
            &root,
            &mut |_, _, _, _, _| Ok(()),
        )
        .unwrap();
        drop(crate::store_query::QueryState::open(&root).unwrap());
        db.execute_batch(
            "CREATE TABLE settings(value TEXT); INSERT INTO settings VALUES('preserved');",
        )
        .unwrap();
        let epoch: Vec<u8> = db
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for action in [
            DiskUsageAction::Inspect,
            DiskUsageAction::ClearCache,
            DiskUsageAction::Optimize,
        ] {
            let report = disk_usage_paths(&root, &cache, action).unwrap();
            assert_eq!(report.clients[0].record_count, 2);
            assert_eq!(report.clients[0].first_timestamp_ms, Some(100));
            assert!(report.usage_bytes > 0);
            assert_eq!(report.recovery_bytes, 0);
            assert!(!root.join(RECOVERY).exists());
        }
        assert!(!cache.join(PRICING[0]).exists());
        assert_eq!(
            fs::read(cache.join("source-message-cache.bin")).unwrap(),
            b"only-copy"
        );
        let report = disk_usage_paths(&root, &cache, DiskUsageAction::ResetUsage).unwrap();
        assert!(report.clients.is_empty());
        assert_eq!(report.recovery_bytes, 0);
        assert!(!root.join(RECOVERY).exists());
        let next: Vec<u8> = db
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(epoch, next);
        assert_eq!(
            db.query_row("SELECT value FROM settings", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "preserved"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM oc_messages", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert!(table(&db, "du_reset").unwrap());
        drop(db);
        assert!(disk_usage_paths(&root, &cache, DiskUsageAction::Inspect)
            .unwrap()
            .clients
            .is_empty());
    }

    #[test]
    fn reset_rejects_old_scope_and_pre_reset_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("usage");
        let context = crate::ResolvedLocalSourceContext::capture(
            Some(tmp.path().to_path_buf()),
            false,
            Default::default(),
        )
        .unwrap();
        let query = crate::store_query::QueryState::open(&root).unwrap();
        let snapshot = query
            .begin_scope(
                &root,
                &context,
                &[],
                None,
                crate::LocalUsageSelection::Report,
            )
            .unwrap();
        let db = query.connection();
        // begin_scope leaves an unpublished generation. Make this synthetic
        // registry a complete published scope before validating its references.
        db.execute_batch(
            "UPDATE sq_scope_generations SET complete=1;
             UPDATE sq_scopes SET current_generation=(
                 SELECT id FROM sq_scope_generations WHERE scope_id=sq_scopes.id LIMIT 1
             );"
        ).unwrap();
        for (table, key, owner) in [
            ("sq_scope_source_current", "scope_id", "SELECT id FROM sq_scopes LIMIT 1"),
            ("sq_scope_source_pins", "scope_generation", "SELECT id FROM sq_scope_generations LIMIT 1"),
        ] {
            db.execute_batch(&format!(
                "INSERT INTO {table}({key},kind,lane,source_order,path,tag,headless,phase)
                 SELECT ({owner}),'synthetic','claude',0,x'01',0,0,0"
            )).unwrap();
        }
        crate::usage_store::validate_managed_schema_readonly(db).unwrap();
        crate::usage_store::refresh_retained_recovery(query.connection(), &root).unwrap();
        let old_backup = fs::read(root.join(RECOVERY)).unwrap();
        reset(query.connection(), &root).unwrap();
        assert_eq!(fs::read(root.join(RECOVERY)).unwrap(), old_backup);
        for table in ["sq_scope_source_current", "sq_scope_source_pins", "sq_scope_sources"] {
            assert_eq!(query.connection().query_row(
                &format!("SELECT count(*) FROM {table}"), [], |r| r.get::<_, i64>(0),
            ).unwrap(), 0);
        }
        let fresh = crate::store_query::QueryState::open(&root).unwrap();
        assert!(fresh.pin(&snapshot).unwrap_err().contains("replaced store"));
        drop(fresh);
        drop(query);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let path = root.join(format!("{DATABASE}{suffix}"));
            if path.exists() {
                fs::rename(&path, root.join(format!("preserved{suffix}"))).unwrap();
            }
        }
        assert!(crate::usage_store::recover_database(&root)
            .unwrap_err()
            .contains("predates usage reset"));
        assert!(!root.join(DATABASE).exists());
        assert!(crate::usage_migration::prepare(&root, &[])
            .unwrap_err()
            .contains("refusing legacy"));
    }
}
