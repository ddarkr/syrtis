//! Canonical compact usage facts and disk-backed OpenCode selection.
//!
//! Source snapshots end before collapse and selection. A failed source transfer
//! rolls back its entire generation; neither a read failure nor a missing path
//! is a successful empty scan. Only explicit compatibility APIs hydrate aliases.

use crate::message_cache::{CachedPath, SourceFingerprint};
use crate::sessions::opencode::{self, OpenCodeCandidate};
use crate::sessions::{CostSource, UnifiedMessage};
use bincode::Options;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DATABASE: &str = "source-message-cache.sqlite3";
const RECOVERY: &str = "source-message-cache.recovery.sqlite3";
const APPLICATION_ID: i64 = 0x54534b55;
const SCHEMA_VERSION: i64 = 1;
const BATCH_ROWS: usize = 1024;
const BATCH_BYTES: usize = 8 * 1024 * 1024;

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|error| match error {
        rusqlite::Error::SqliteFailure(code, _) => format!(
            "usage store database operation failed (SQLite {:?}, extended code {})",
            code.code, code.extended_code,
        ),
        _ => "usage store database operation failed".into(),
    })
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    bincode::serialize(value).map_err(|_| "usage store encoding failed".into())
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    bincode::deserialize(bytes).map_err(|_| "usage store decoding failed".into())
}

fn decode_row<T: DeserializeOwned>(row: &rusqlite::Row<'_>, column: usize) -> Result<T, String> {
    let value = sql(row.get_ref(column))?;
    decode(
        value
            .as_blob()
            .map_err(|_| "usage store compact row has an invalid type")?,
    )
}

fn secure_directory(root: &Path) -> Result<(), String> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err("usage store directory is unsafe".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|_| "usage store directory creation failed")?;
        }
        Err(_) => return Err("usage store directory admission failed".into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta =
            fs::symlink_metadata(root).map_err(|_| "usage store directory admission failed")?;
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err("usage store directory ownership is unsafe".into());
        }
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))
                .map_err(|_| "usage store directory protection failed")?;
        }
    }
    Ok(())
}

fn secure_file(path: &Path) -> Result<(), String> {
    admit_file(path, true)
}

// A raw descriptor close on an existing SQLite main/SHM inode releases this
// process's POSIX locks behind SQLite's unixInodeInfo registry. Existing-file
// admission must therefore use metadata only; creation here is for lock files.
#[cfg(unix)]
fn admit_file(path: &Path, create: bool) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(file) => drop(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("usage store file creation failed".into()),
            }
            fs::symlink_metadata(path).map_err(|_| "usage store file admission failed")?
        }
        Err(_) => return Err("usage store file admission failed".into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("usage store file is unsafe".into());
    }
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
        return Err("usage store file ownership is unsafe".into());
    }
    if metadata.mode() & 0o777 != 0o600 {
        let native = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "usage store file path is invalid")?;
        // Never chmod a symlink that replaced the admitted path. Unsupported
        // nofollow protection fails closed, rather than following it.
        if unsafe {
            libc::fchmodat(
                libc::AT_FDCWD,
                native.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err("usage store file protection failed".into());
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn admit_file(path: &Path, create: bool) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || !m.is_file()) {
        return Err("usage store file is unsafe".into());
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("usage store file admission failed".into()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file
            .metadata()
            .map_err(|_| "usage store file admission failed")?;
        if !create && metadata.nlink() == 0 {
            return Ok(());
        }
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err("usage store file ownership is unsafe".into());
        }
        if metadata.permissions().mode() & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "usage store file protection failed")?;
        }
    }
    Ok(())
}

fn admit_sidecar(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            Err("usage store sidecar is unsafe".into())
        }
        // SQLite may unlink an idle sidecar between admission and open.
        // Never recreate a detached journal or shared-memory file.
        Ok(_) => admit_file(path, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("usage store sidecar admission failed".into()),
    }
}

fn database_path_metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    admit_file(path, false)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("usage store file admission failed".into()),
    }
}

/// Resolve only the already-admitted directory for SQLite's stricter NOFOLLOW
/// opener. Native cache/source scope paths remain unchanged; the final database
/// component is never canonicalized through a symlink.
pub(crate) fn sqlite_open_path(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or("usage store database parent is unavailable")?;
    secure_directory(parent)?;
    let canonical =
        fs::canonicalize(parent).map_err(|_| "usage store database parent resolution failed")?;
    Ok(canonical.join(
        path.file_name()
            .ok_or("usage store database name is unavailable")?,
    ))
}

fn sqlite_status(code: i32) -> Result<(), String> {
    if code == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        sql(Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        )))
    }
}

fn admit_open_database(
    connection: &Connection,
    path: &Path,
    expected: Option<&fs::Metadata>,
) -> Result<(), String> {
    #[cfg(unix)]
    let verify_not_moved = || -> Result<(), String> {
        let mut moved: std::ffi::c_int = 0;
        // The pinned unix VFS compares its actual opened inode with the path.
        // This is a public file-control API, not a private file-struct offset.
        sqlite_status(unsafe {
            rusqlite::ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
                (&mut moved as *mut std::ffi::c_int).cast(),
            )
        })?;
        if moved != 0 {
            return Err("usage store opened file identity changed".into());
        }
        Ok(())
    };
    #[cfg(unix)]
    verify_not_moved()?;
    let current = database_path_metadata(path)?.ok_or("usage store opened file is absent")?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let native_parent = path
            .parent()
            .ok_or("usage store database parent is unavailable")?;
        secure_directory(native_parent)?;
        // SQLite retains the exact canonical path it opened. Compare both the
        // admitted directory and main inode, so ancestor aliases are accepted
        // without permitting a swapped root or final-file symlink.
        let filename =
            unsafe { rusqlite::ffi::sqlite3_db_filename(connection.handle(), c"main".as_ptr()) };
        if filename.is_null() {
            return Err("usage store opened database path is unavailable".into());
        }
        let opened = Path::new(std::ffi::OsStr::from_bytes(
            unsafe { std::ffi::CStr::from_ptr(filename) }.to_bytes(),
        ));
        let opened_parent = opened
            .parent()
            .ok_or("usage store opened database parent is unavailable")?;
        let native_parent_metadata = fs::symlink_metadata(native_parent)
            .map_err(|_| "usage store database parent admission failed")?;
        let opened_parent_metadata = fs::symlink_metadata(opened_parent)
            .map_err(|_| "usage store opened database parent admission failed")?;
        let opened_metadata =
            database_path_metadata(opened)?.ok_or("usage store opened database is absent")?;
        if native_parent_metadata.file_type().is_symlink()
            || native_parent_metadata.dev() != opened_parent_metadata.dev()
            || native_parent_metadata.ino() != opened_parent_metadata.ino()
            || current.dev() != opened_metadata.dev()
            || current.ino() != opened_metadata.ino()
        {
            return Err("usage store opened directory or file identity changed".into());
        }
        if expected
            .is_some_and(|before| before.dev() != current.dev() || before.ino() != current.ino())
            || current.mode() & 0o777 != 0o600
        {
            return Err("usage store opened file identity or protection changed".into());
        }
        verify_not_moved()?;
    }
    #[cfg(not(unix))]
    let _ = (connection, expected, current);
    Ok(())
}

fn sync_database(connection: &Connection) -> Result<(), String> {
    let mut file: *mut rusqlite::ffi::sqlite3_file = std::ptr::null_mut();
    // Borrow SQLite's own VFS file for the lifetime of this synchronous call.
    // Never open/close an unrelated descriptor for a lock-bearing inode.
    sqlite_status(unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut rusqlite::ffi::sqlite3_file).cast(),
        )
    })?;
    let file_ref = unsafe { file.as_ref() }.ok_or("usage store VFS file is unavailable")?;
    let methods =
        unsafe { file_ref.pMethods.as_ref() }.ok_or("usage store VFS methods are unavailable")?;
    let sync = methods.xSync.ok_or("usage store VFS sync is unavailable")?;
    sqlite_status(unsafe { sync(file, rusqlite::ffi::SQLITE_SYNC_FULL) })
}

fn suffix(path: &Path, value: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(value);
    PathBuf::from(name)
}

pub(crate) fn configure_connection(conn: &Connection) -> Result<(), String> {
    sql(conn.busy_timeout(Duration::from_secs(5)))?;
    // RAM over latency: 256 KiB main + 64 KiB temp soft budgets (not a hard
    // process cap), coherent with source_watch's 64 KiB precedent.
    sql(conn.execute_batch("PRAGMA cache_size=-256; PRAGMA temp_store=FILE; PRAGMA temp.cache_size=-64; PRAGMA mmap_size=0; PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON;"))
}

pub(crate) fn admit_schema(conn: &Connection) -> Result<(), String> {
    let application: i64 = sql(conn.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(conn.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if application == APPLICATION_ID && version == SCHEMA_VERSION {
        return Ok(());
    }
    let transaction = sql(rusqlite::Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    ))?;
    // Another opener may have initialized the database after our warm probe.
    let application: i64 =
        sql(transaction.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(transaction.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if application == APPLICATION_ID && version == SCHEMA_VERSION {
        return sql(transaction.commit());
    }
    let count: i64 = sql(transaction.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    ))?;
    if application != 0 || version != 0 || count != 0 {
        return Err("usage store schema is unsupported; existing data preserved".into());
    }
    sql(transaction.pragma_update(None, "application_id", APPLICATION_ID))?;
    sql(transaction.pragma_update(None, "user_version", SCHEMA_VERSION))?;
    sql(transaction.commit())
}

fn lock_file(path: &Path) -> Result<fs::File, String> {
    secure_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "usage store lock admission failed")?;
    fs2::FileExt::lock_exclusive(&file).map_err(|_| "usage store lock failed")?;
    Ok(file)
}

/// Common secure database opener. Admission is metadata-only on warm opens;
/// corruption is reported by actual reads rather than a whole-store scan.
pub(crate) fn open_database(cache_root: &Path) -> Result<Connection, String> {
    secure_directory(cache_root)?;
    let path = cache_root.join(DATABASE);
    let admitted = database_path_metadata(&path)?;
    let sqlite_path = sqlite_open_path(&path)?;
    for extension in ["-wal", "-shm", "-journal"] {
        admit_sidecar(&suffix(&path, extension))?;
    }
    let connect = || -> Result<Connection, String> {
        let conn = sql(Connection::open_with_flags(
            &sqlite_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        ))
        .map_err(|error| format!("opening usage store: {error}"))?;
        admit_open_database(&conn, &path, admitted.as_ref())?;
        configure_connection(&conn).map_err(|error| format!("configuring usage store: {error}"))?;
        Ok(conn)
    };
    let mut conn = connect()?;
    let journal: String = sql(conn.pragma_query_value(None, "journal_mode", |r| r.get(0)))
        .map_err(|error| format!("reading usage store journal mode: {error}"))?;
    if journal != "wal" {
        // The rollback-to-WAL transition upgrades a rollback reader to a writer.
        // Competing cold openers must not hold SQLite connections while waiting
        // for that upgrade. Warm WAL admission and all queries stay lock-free.
        drop(conn);
        let _initialization = lock_file(&cache_root.join(format!("{DATABASE}.init.lock")))?;
        conn = connect()?;
        admit_schema(&conn).map_err(|error| format!("initializing usage store schema: {error}"))?;
        let journal: String = sql(conn.pragma_query_value(None, "journal_mode", |r| r.get(0)))
            .map_err(|error| format!("rechecking usage store journal mode: {error}"))?;
        if journal != "wal" {
            let selected: String =
                sql(conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0)))
                    .map_err(|error| format!("initializing usage store WAL: {error}"))?;
            if selected != "wal" {
                return Err("usage store WAL mode is unavailable; existing data preserved".into());
            }
        }
    } else {
        admit_schema(&conn).map_err(|error| format!("admitting usage store schema: {error}"))?;
    }
    sql(conn.pragma_update(None, "synchronous", "FULL"))
        .map_err(|error| format!("configuring usage store durability: {error}"))?;
    admit_open_database(&conn, &path, admitted.as_ref())?;
    Ok(conn)
}

/// Read-only admission for storage inspection: never create or migrate.
pub(crate) fn open_existing_database(root: &Path) -> Result<Connection, String> {
    let path = root.join(DATABASE);
    let admitted = database_path_metadata(&path)?.ok_or("usage store is absent")?;
    for extension in ["-wal", "-shm", "-journal"] {
        admit_sidecar(&suffix(&path, extension))?;
    }
    let conn = sql(Connection::open_with_flags(
        sqlite_open_path(&path)?,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ))?;
    admit_open_database(&conn, &path, Some(&admitted))?;
    configure_connection(&conn)?;
    let application: i64 = sql(conn.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(conn.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if application != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err("usage store schema is unsupported; existing data preserved".into());
    }
    Ok(conn)
}

pub(crate) fn management_lock(root: &Path, exclusive: bool) -> Result<fs::File, String> {
    secure_directory(root)?;
    let path = root.join("usage-management.lock");
    secure_file(&path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "usage management lock admission failed")?;
    if exclusive {
        fs2::FileExt::lock_exclusive(&file)
    } else {
        fs2::FileExt::lock_shared(&file)
    }
    .map_err(|_| "usage management lock failed")?;
    Ok(file)
}

pub(crate) fn write_reset_barrier(root: &Path, epoch: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let path = root.join("usage-reset.barrier");
    secure_file(&path)?;
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| "usage reset barrier admission failed")?;
    file.write_all(epoch)
        .and_then(|_| file.sync_all())
        .map_err(|_| "usage reset barrier sync failed")?;
    #[cfg(unix)]
    fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|_| "usage reset directory sync failed")?;
    Ok(())
}

/// Called only after retained-history semantic changes, never on warm queries.
/// SQLite's destination transaction preserves the previous recovery snapshot if
/// the backup fails. It is never queried as a second report source.
pub(crate) fn refresh_retained_recovery(
    conn: &Connection,
    cache_root: &Path,
) -> Result<(), String> {
    secure_directory(cache_root)?;
    // Only recovery writers contend here; canonical readers and ingestion
    // remain independent. A later writer cannot overwrite a newer backup.
    let _recovery = lock_file(&cache_root.join(format!("{RECOVERY}.lock")))?;
    let path = cache_root.join(RECOVERY);
    let admitted = database_path_metadata(&path)?;
    let sqlite_path = sqlite_open_path(&path)?;
    for extension in ["-wal", "-shm", "-journal"] {
        admit_sidecar(&suffix(&path, extension))?;
    }
    let mut destination = sql(Connection::open_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ))?;
    admit_open_database(&destination, &path, admitted.as_ref())?;
    configure_connection(&destination)?;
    sql(destination.pragma_update(None, "journal_mode", "DELETE"))?;
    sql(destination.pragma_update(None, "synchronous", "FULL"))?;
    let backup = sql(rusqlite::backup::Backup::new(conn, &mut destination))?;
    finish_backup(&backup)?;
    drop(backup);
    // Backup copies page one's WAL header from the canonical source. Reopen
    // before normalizing it: the destination pager still remembers DELETE.
    // Otherwise the next reader enables WAL and a retry cannot switch modes
    // while that reader remains open, even after its transaction rolled back.
    drop(destination);
    let destination = sql(Connection::open_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ))?;
    admit_open_database(&destination, &path, admitted.as_ref())?;
    configure_connection(&destination)?;
    sql(destination.pragma_update(None, "journal_mode", "DELETE"))?;
    sync_database(&destination)?;
    admit_open_database(&destination, &path, admitted.as_ref())?;
    Ok(())
}

/// Explicit offline recovery only. Never overwrite a live/broken canonical DB
/// or replay its WAL into a restored generation. The operator must preserve and
/// move all broken files first, so even invalid retained history remains intact.
pub(crate) fn recover_database(cache_root: &Path) -> Result<(), String> {
    secure_directory(cache_root)?;
    let _management = management_lock(cache_root, true)?;
    let target = cache_root.join(DATABASE);
    for path in [
        target.clone(),
        suffix(&target, "-wal"),
        suffix(&target, "-shm"),
        suffix(&target, "-journal"),
    ] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(
                    "usage store recovery requires an absent canonical database and sidecars"
                        .into(),
                )
            }
        }
    }
    let recovery = cache_root.join(RECOVERY);
    let admitted =
        database_path_metadata(&recovery)?.ok_or("usage store recovery snapshot is absent")?;
    let sqlite_path = sqlite_open_path(&recovery)?;
    for extension in ["-wal", "-shm", "-journal"] {
        admit_sidecar(&suffix(&recovery, extension))?;
    }
    let source = sql(Connection::open_with_flags(
        &sqlite_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    ))?;
    admit_open_database(&source, &recovery, Some(&admitted))?;
    let barrier = cache_root.join("usage-reset.barrier");
    if database_path_metadata(&barrier)?.is_some() {
        let expected = fs::read(&barrier).map_err(|_| "usage reset barrier read failed")?;
        let actual: Vec<u8> = source
            .query_row("SELECT epoch FROM du_reset", [], |r| r.get(0))
            .map_err(|_| "recovery predates usage reset; preserved without restoring")?;
        if expected.len() != 16 || expected != actual {
            return Err("recovery predates usage reset; preserved without restoring".into());
        }
    }
    configure_connection(&source)?;
    let result: String = sql(source.query_row("PRAGMA quick_check", [], |r| r.get(0)))?;
    if result != "ok" {
        return Err("usage store recovery snapshot is invalid".into());
    }
    let app: i64 = sql(source.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(source.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if app != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err("usage store recovery schema is unsupported".into());
    }
    // Restore off to the side. No failed backup may leave an empty canonical
    // file that a later opener could mistake for a newly created cache.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "usage store recovery clock failed")?
        .as_nanos();
    let staging = cache_root.join(format!(
        ".usage-restore-{}-{nonce}.sqlite3",
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
        .map_err(|_| "usage store recovery destination creation failed")?;
    let result = (|| {
        let admitted = database_path_metadata(&staging)?;
        let sqlite_path = sqlite_open_path(&staging)?;
        let mut destination = sql(Connection::open_with_flags(
            &sqlite_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        ))?;
        admit_open_database(&destination, &staging, admitted.as_ref())?;
        configure_connection(&destination)?;
        sql(destination.pragma_update(None, "synchronous", "FULL"))?;
        let backup = sql(rusqlite::backup::Backup::new(&source, &mut destination))?;
        finish_backup(&backup)?;
        drop(backup);
        sync_database(&destination)?;
        admit_open_database(&destination, &staging, admitted.as_ref())?;
        drop(destination);
        // Atomic no-replace publication, also rejecting a concurrent opener.
        fs::hard_link(&staging, &target).map_err(|_| "usage store recovery publication failed")?;
        #[cfg(unix)]
        fs::File::open(cache_root)
            .and_then(|file| file.sync_all())
            .map_err(|_| "usage store recovery directory sync failed")?;
        Ok(())
    })();
    let _ = fs::remove_file(staging);
    result
}

fn finish_backup(backup: &rusqlite::backup::Backup<'_, '_>) -> Result<(), String> {
    let started = std::time::Instant::now();
    loop {
        match sql(backup.step(128))? {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More if started.elapsed() < Duration::from_secs(300) => {}
            rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                return Err("usage store recovery backup is busy".into());
            }
            _ => return Err("usage store recovery backup deadline exceeded".into()),
        }
    }
}

fn schema(conn: &Connection) -> Result<(), String> {
    let exists: bool = sql(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='oc_meta')",
        [],
        |r| r.get(0),
    ))?;
    if !exists {
        sql(conn.execute_batch("BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS oc_meta(version INTEGER NOT NULL);
        INSERT INTO oc_meta SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM oc_meta);
        CREATE TABLE IF NOT EXISTS oc_sources(path BLOB PRIMARY KEY, fingerprint BLOB NOT NULL, kind INTEGER NOT NULL, generation BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS oc_generations(generation BLOB PRIMARY KEY, path BLOB NOT NULL, fingerprint BLOB NOT NULL, kind INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS oc_rows(source BLOB NOT NULL, ordinal INTEGER NOT NULL, candidate BLOB NOT NULL, PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS oc_messages(source BLOB NOT NULL, ordinal INTEGER NOT NULL, fingerprint BLOB NOT NULL, embedded_id TEXT, v1_fallback INTEGER NOT NULL, workspace_conflict INTEGER NOT NULL, message BLOB NOT NULL, payload BLOB NOT NULL, timestamp INTEGER NOT NULL, provider INTEGER NOT NULL, primary_key TEXT, PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS oc_fingerprint ON oc_messages(source,fingerprint,ordinal);
        CREATE INDEX IF NOT EXISTS oc_fingerprint_id ON oc_messages(source,fingerprint,embedded_id,ordinal);
        CREATE TABLE IF NOT EXISTS oc_aliases(source BLOB NOT NULL, ordinal INTEGER NOT NULL, position INTEGER NOT NULL, key TEXT NOT NULL, PRIMARY KEY(source,ordinal,key)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS oc_alias_order ON oc_aliases(source,ordinal,position);
        COMMIT;"))?;
    }
    let version: i64 = sql(conn.query_row("SELECT version FROM oc_meta", [], |r| r.get(0)))?;
    if version != 1 {
        return Err("OpenCode usage store schema unsupported".into());
    }
    Ok(())
}

fn source_key(path: &Path) -> Result<Vec<u8>, String> {
    // Native source descriptors match the cache/registry path codec. Compact
    // usage rows and opaque generation keys retain fixed-int encoding.
    bincode::options()
        .serialize(&CachedPath::from_path(path))
        .map_err(|_| "usage store path encoding failed".into())
}

fn source_fingerprint(path: &Path, sqlite: bool) -> Result<SourceFingerprint, String> {
    let value = if sqlite {
        SourceFingerprint::from_sqlite_path(path)
    } else {
        SourceFingerprint::from_path_samples_only(path)
    };
    value.ok_or_else(|| "OpenCode source fingerprint failed".into())
}

fn readonly_source(path: &Path) -> Result<Connection, String> {
    let conn = sql(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ))?;
    configure_connection(&conn)?;
    sql(conn.pragma_update(None, "query_only", true))?;
    Ok(conn)
}

// Each handoff is one compact row, stronger than both batch limits. Savepoints
// delimit bounded write batches without publishing a partial source generation.
struct SourceWriter<'a> {
    conn: &'a Connection,
    source: &'a [u8],
    ordinal: i64,
    rows: usize,
    bytes: usize,
}
impl SourceWriter<'_> {
    fn push(&mut self, candidate: OpenCodeCandidate) -> Result<(), String> {
        let bytes = encode(&candidate)?;
        if self.rows > 0
            && (self.rows == BATCH_ROWS || self.bytes.saturating_add(bytes.len()) > BATCH_BYTES)
        {
            sql(self
                .conn
                .execute_batch("RELEASE oc_batch; SAVEPOINT oc_batch;"))?;
            self.rows = 0;
            self.bytes = 0;
        }
        // A single oversized compact row is processed alone, never dropped.
        sql(self.conn.execute(
            "INSERT INTO oc_rows VALUES(?1,?2,?3)",
            params![self.source, self.ordinal, bytes],
        ))?;
        self.ordinal += 1;
        self.rows += 1;
        self.bytes = self.bytes.saturating_add(bytes.len());
        Ok(())
    }
}

/// A fresh transfer supersedes live rows but never erases canonical history:
/// candidates missing from the new read are carried from the previous
/// generation of the same (path, kind) identity, keyed by stable dedup key.
/// Same-key updates use the fresh observation; removed-row facts persist.
/// Fresh membership is a FILE TEMP key-or-alias table and prior rows stream
/// in bounded ordinal pages; carried rows never rejoin the membership set.
fn carry_missing_opencode_candidates(
    conn: &Connection,
    key: &[u8],
    previous: &[u8],
) -> Result<(), String> {
    // Guard before any decode: same-generation syncs carry nothing.
    if previous == key {
        return Ok(());
    }
    let has_prior: bool = sql(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM oc_rows WHERE source=?1)",
        [previous],
        |row| row.get(0),
    ))?;
    if !has_prior {
        return Ok(());
    }
    // Fresh cohort key-or-alias membership only, on FILE TEMP so no
    // whole-history Rust set exists. Carried rows never join it.
    sql(conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS oc_carry_live(key TEXT PRIMARY KEY) WITHOUT ROWID; DELETE FROM oc_carry_live;",
    ))?;
    sql(conn.execute_batch("SAVEPOINT oc_carry"))?;
    let result = (|| -> Result<(), String> {
        // Seal the fresh cohort before any append shares this source key.
        let mut scan =
            sql(conn.prepare("SELECT candidate FROM oc_rows WHERE source=?1 ORDER BY ordinal"))?;
        let mut scan_rows = sql(scan.query([key]))?;
        while let Some(row) = sql(scan_rows.next())? {
            let candidate: OpenCodeCandidate = decode_row(row, 0)?;
            if let Some(identity) = candidate.message.dedup_key.as_deref() {
                sql(conn.execute(
                    "INSERT OR IGNORE INTO temp.oc_carry_live VALUES(?1)",
                    [identity],
                ))?;
            }
            for alias in &candidate.message.dedup_aliases {
                sql(conn.execute(
                    "INSERT OR IGNORE INTO temp.oc_carry_live VALUES(?1)",
                    [alias],
                ))?;
            }
        }
        drop(scan_rows);
        drop(scan);
        // Bounded SQL row appends under the outer oc_carry savepoint: no
        // SourceWriter oc_batch savepoint, so >1024 carried rows stay valid.
        let mut next_ordinal: i64 = sql(conn.query_row(
            "SELECT COALESCE(MAX(ordinal),-1)+1 FROM oc_rows WHERE source=?1",
            [key],
            |row| row.get(0),
        ))?;
        // Ordinal-paged prior stream: each SELECT finishes before any INSERT,
        // and appends use the fresh source key, so they never re-enter the
        // previous-generation scan. A page of 64 bounds decoded bytes per
        // iteration; gaps are fine because paging resumes after the last
        // seen ordinal.
        let mut after: i64 = -1;
        loop {
            let batch: Vec<(i64, OpenCodeCandidate)> = {
                let mut paged = sql(conn.prepare(
                    "SELECT ordinal,candidate FROM oc_rows WHERE source=?1 AND ordinal>?2 ORDER BY ordinal LIMIT 64",
                ))?;
                let mut paged_rows = sql(paged.query(params![previous, after]))?;
                let mut batch = Vec::new();
                while let Some(row) = sql(paged_rows.next())? {
                    let ordinal: i64 = sql(row.get(0))?;
                    batch.push((ordinal, decode_row(row, 1)?));
                }
                drop(paged_rows);
                drop(paged);
                batch
            };
            if batch.is_empty() {
                break;
            }
            for (ordinal, candidate) in batch {
                after = ordinal;
                let mut seen = false;
                if let Some(identity) = candidate.message.dedup_key.as_deref() {
                    seen = sql(conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM temp.oc_carry_live WHERE key=?1)",
                        [identity],
                        |row| row.get(0),
                    ))?;
                }
                for alias in &candidate.message.dedup_aliases {
                    if seen {
                        break;
                    }
                    seen = sql(conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM temp.oc_carry_live WHERE key=?1)",
                        [alias],
                        |row| row.get(0),
                    ))?;
                }
                if !seen {
                    sql(conn.execute(
                        "INSERT INTO oc_rows VALUES(?1,?2,?3)",
                        params![key, next_ordinal, encode(&candidate)?],
                    ))?;
                    next_ordinal += 1;
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => sql(conn.execute_batch("RELEASE oc_carry")),
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK TO oc_carry; RELEASE oc_carry");
            sql(conn.execute_batch("DROP TABLE IF EXISTS temp.oc_carry_live"))?;
            Err(error)
        }
    }?;
    sql(conn.execute_batch("DROP TABLE IF EXISTS temp.oc_carry_live"))
}

fn transfer_source(conn: &Connection, path: &Path, key: &[u8], sqlite: bool) -> Result<(), String> {
    sql(conn.execute("DELETE FROM oc_rows WHERE source=?1", [key]))?;
    sql(conn.execute_batch("SAVEPOINT oc_batch"))?;
    let mut writer = SourceWriter {
        conn,
        source: key,
        ordinal: 0,
        rows: 0,
        bytes: 0,
    };
    if sqlite {
        let mut source = readonly_source(path)?;
        let temp_store: i64 = sql(conn.pragma_query_value(None, "temp_store", |row| row.get(0)))?;
        if temp_store == 2 {
            // Explicit Vec materialization is intentionally memory-resident;
            // its source-side sort/temp work must not depend on scratch disk.
            sql(source.pragma_update(None, "temp_store", "MEMORY"))?;
        }
        let snapshot = sql(source.transaction())?;
        // Force the schema and all rows onto one SQLite read snapshot.
        opencode::visit_opencode_sqlite_rows(&snapshot, &mut |candidate| writer.push(candidate))?;
        sql(snapshot.commit())?;
        drop(source);
    } else {
        // File read errors are not a successfully parsed empty generation.
        let bytes = fs::read(path).map_err(|_| "OpenCode JSON source read failed")?;
        // Reuse exact file acceptance and construction; no second file read.
        if let Some(candidate) = opencode::parse_opencode_file_candidate(path, bytes) {
            writer.push(candidate)?;
        }
    }
    sql(conn.execute_batch("RELEASE oc_batch"))
}

fn add_alias(conn: &Connection, key: &[u8], ordinal: i64, alias: &str) -> Result<(), String> {
    sql(conn.execute("INSERT OR IGNORE INTO oc_aliases SELECT ?1,?2,coalesce(max(position)+1,0),?3 FROM oc_aliases WHERE source=?1 AND ordinal=?2", params![key,ordinal,alias]))?;
    Ok(())
}

struct CollapsedMessage {
    ordinal: i64,
    embedded: Option<String>,
    fallback: bool,
    conflict: bool,
    message: UnifiedMessage,
}

fn collapse_source(conn: &Connection, key: &[u8], sqlite: bool) -> Result<(), String> {
    sql(conn.execute("DELETE FROM oc_messages WHERE source=?1", [key]))?;
    sql(conn.execute("DELETE FROM oc_aliases WHERE source=?1", [key]))?;
    let mut statement = sql(
        conn.prepare("SELECT ordinal,candidate FROM oc_rows WHERE source=?1 ORDER BY ordinal")
    )?;
    let mut rows = sql(statement.query([key]))?;
    while let Some(row) = sql(rows.next())? {
        let ordinal: i64 = sql(row.get(0))?;
        let candidate: OpenCodeCandidate = decode_row(row, 1)?;
        let fingerprint = encode(&candidate.fingerprint)?;
        let found: Option<CollapsedMessage> = if sqlite {
            let slot: Option<i64> = match candidate.embedded_id.as_deref() {
                Some(id) => sql(conn.query_row("SELECT min(ordinal) FROM (SELECT min(ordinal) AS ordinal FROM oc_messages WHERE source=?1 AND fingerprint=?2 AND embedded_id IS NULL UNION ALL SELECT min(ordinal) FROM oc_messages WHERE source=?1 AND fingerprint=?2 AND embedded_id=?3)", params![key,fingerprint,id], |r| r.get(0)))?,
                None => sql(conn.query_row("SELECT ordinal FROM oc_messages WHERE source=?1 AND fingerprint=?2 ORDER BY ordinal LIMIT 1", params![key,fingerprint], |r| r.get(0)).optional())?,
            };
            match slot {
                Some(slot) => Some(sql(conn.query_row("SELECT ordinal,embedded_id,v1_fallback,workspace_conflict,message FROM oc_messages WHERE source=?1 AND ordinal=?2", params![key,slot], |r| Ok(CollapsedMessage {ordinal:r.get(0)?,embedded:r.get(1)?,fallback:r.get(2)?,conflict:r.get(3)?,message:decode_row(r,4).map_err(|error|rusqlite::Error::FromSqlConversionFailure(4,rusqlite::types::Type::Blob,Box::new(std::io::Error::other(error))))?})))?),
                None => None,
            }
        } else {
            None
        };
        if let Some(CollapsedMessage {
            ordinal: slot,
            mut embedded,
            mut fallback,
            mut conflict,
            mut message,
        }) = found
        {
            let promote_id = candidate.embedded_id.is_some() && embedded.is_none();
            let promote_v1 = candidate.embedded_id.is_none()
                && candidate.is_v1
                && embedded.is_none()
                && !fallback;
            if promote_id {
                embedded = candidate.embedded_id.clone();
            }
            if candidate.embedded_id.is_none() && candidate.is_v1 {
                fallback = true;
            }
            if let Some(incoming) = &candidate.message.dedup_key {
                if message.dedup_key.as_ref() != Some(incoming) {
                    if promote_id || promote_v1 {
                        if let Some(previous) = &message.dedup_key {
                            sql(conn.execute("UPDATE oc_aliases SET position=(SELECT coalesce(max(position)+1,0) FROM oc_aliases WHERE source=?1 AND ordinal=?2) WHERE source=?1 AND ordinal=?2 AND key=?3",params![key,slot,previous]))?;
                        }
                        sql(conn.execute("INSERT INTO oc_aliases VALUES(?1,?2,-1,?3) ON CONFLICT(source,ordinal,key) DO UPDATE SET position=-1",params![key,slot,incoming]))?;
                        message.dedup_key = Some(incoming.clone());
                    } else {
                        add_alias(conn, key, slot, incoming)?;
                    }
                }
            }
            if message.cost_source != CostSource::ProviderReported
                && candidate.message.cost_source == CostSource::ProviderReported
            {
                message.cost = candidate.message.cost;
                message.cost_source = candidate.message.cost_source;
            }
            if !conflict {
                match (&message.workspace_key, &candidate.message.workspace_key) {
                    (None, Some(_)) => message.set_workspace(
                        candidate.message.workspace_key.clone(),
                        candidate.message.workspace_label.clone(),
                    ),
                    (Some(existing), Some(incoming)) if existing != incoming => {
                        conflict = true;
                        message.set_workspace(None, None);
                    }
                    _ => {}
                }
            }
            sql(conn.execute("UPDATE oc_messages SET embedded_id=?3,v1_fallback=?4,workspace_conflict=?5,message=?6,provider=?7,primary_key=?8 WHERE source=?1 AND ordinal=?2", params![key,slot,embedded,fallback,conflict,encode(&message)?,message.cost_source==CostSource::ProviderReported,message.dedup_key]))?;
        } else {
            sql(conn.execute(
                "INSERT INTO oc_messages VALUES(?1,?2,?3,?4,?5,0,?6,?7,?8,?9,?10)",
                params![
                    key,
                    ordinal,
                    fingerprint,
                    candidate.embedded_id,
                    candidate.is_v1 && candidate.embedded_id.is_none(),
                    encode(&candidate.message)?,
                    payload(&candidate.message)?,
                    candidate.message.timestamp,
                    candidate.message.cost_source == CostSource::ProviderReported,
                    candidate.message.dedup_key
                ],
            ))?;
            if let Some(alias) = &candidate.message.dedup_key {
                sql(conn.execute(
                    "INSERT INTO oc_aliases VALUES(?1,?2,-1,?3)",
                    params![key, ordinal, alias],
                ))?;
            }
        }
    }
    Ok(())
}

fn sync_source(
    conn: &mut Connection,
    path: &Path,
    sqlite: bool,
    register: &mut impl FnMut(&Connection, &[u8]) -> Result<(), String>,
) -> Result<Vec<u8>, String> {
    // Absent sources keep durable facts: re-register the last-known
    // generation so the scope still references its canonical rows. A
    // present-but-unfingerprintable source still fails closed below.
    if source_fingerprint(path, sqlite).is_err()
        && fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        let transaction = sql(conn.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let current: Option<Vec<u8>> = sql(transaction
            .query_row(
                "SELECT generation FROM oc_sources WHERE path=?1",
                [&source_key(path)?],
                |r| r.get(0),
            )
            .optional())?;
        if let Some(generation) = current {
            register(&transaction, &generation)?;
            sql(transaction.commit())?;
            return Ok(generation);
        }
        sql(transaction.rollback())?;
    }
    let path_key = source_key(path)?;
    for _ in 0..2 {
        let fingerprint = source_fingerprint(path, sqlite)?;
        let fingerprint_bytes = encode(&fingerprint)?;
        let generation = encode(&(&path_key, &fingerprint_bytes, sqlite))?;
        let transaction = sql(conn.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let current: Option<(Vec<u8>, bool)> = sql(transaction
            .query_row(
                "SELECT fingerprint,kind FROM oc_sources WHERE path=?1",
                [&path_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional())?;
        if current
            .as_ref()
            .is_some_and(|(stored, kind)| stored == &fingerprint_bytes && *kind == sqlite)
        {
            register(&transaction, &generation)?;
            sql(transaction.commit())?;
            return Ok(generation);
        }
        if source_fingerprint(path, sqlite)? != fingerprint {
            continue;
        }
        // An emptied live source (zero transferable rows) is not new
        // information: retry the transfer once against the prior generation
        // and keep canonical history when the retry also yields nothing.
        // Genuinely new empty sources (no prior generation) publish empty.
        let exists: bool = sql(transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM oc_generations WHERE generation=?1)",
            [&generation],
            |r| r.get(0),
        ))?;
        if !exists {
            let previous: Option<Vec<u8>> = sql(transaction
                .query_row(
                    "SELECT generation FROM oc_sources WHERE path=?1",
                    [&path_key],
                    |r| r.get(0),
                )
                .optional())?;
            transfer_source(&transaction, path, &generation, sqlite)?;
            // The raw source snapshot is released before collapse/selection.
            #[cfg(test)]
            tests::run_transfer_hook(path);
            if source_fingerprint(path, sqlite)? != fingerprint {
                continue;
            }
            // Removed-row facts persist: carry candidates the fresh read no
            // longer contains from the previous same-identity generation.
            // A fully emptied live source keeps its previous generation (no
            // new information); a genuinely new empty source publishes empty.
            let transferred: i64 = sql(transaction.query_row(
                "SELECT COUNT(*) FROM oc_rows WHERE source=?1",
                [&generation],
                |r| r.get(0),
            ))?;
            if transferred == 0 {
                if let Some(previous) = previous.as_deref() {
                    sql(transaction.execute("DELETE FROM oc_rows WHERE source=?1", [&generation]))?;
                    register(&transaction, previous)?;
                    sql(transaction.commit())?;
                    return Ok(previous.to_vec());
                }
            } else if let Some(previous) = previous.as_deref() {
                carry_missing_opencode_candidates(&transaction, &generation, previous)?;
            }
            collapse_source(&transaction, &generation, sqlite)?;
            sql(transaction.execute(
                "INSERT INTO oc_generations VALUES(?1,?2,?3,?4)",
                params![generation, path_key, fingerprint_bytes, sqlite],
            ))?;
        }
        sql(transaction.execute("INSERT INTO oc_sources VALUES(?1,?2,?3,?4) ON CONFLICT(path) DO UPDATE SET fingerprint=excluded.fingerprint,kind=excluded.kind,generation=excluded.generation", params![path_key,fingerprint_bytes,sqlite,generation]))?;
        register(&transaction, &generation)?;
        sql(transaction.commit())?;
        return Ok(generation);
    }
    Err("OpenCode source changed during snapshot; previous generation preserved".into())
}

/// Explicit Vec materialization uses the same collapse engine entirely in
/// memory. Only production synchronization/query storage is file-backed.
pub(crate) fn materialize_opencode_source(path: &Path) -> Result<Vec<UnifiedMessage>, String> {
    let mut conn = materialization_database()?;
    schema(&conn)?;
    let key = sync_source(&mut conn, path, true, &mut |_, _| Ok(()))?;
    let mut result = Vec::new();
    let mut statement =
        sql(conn
            .prepare("SELECT ordinal,message FROM oc_messages WHERE source=?1 ORDER BY ordinal"))?;
    let mut rows = sql(statement.query([&key]))?;
    while let Some(row) = sql(rows.next())? {
        let ordinal: i64 = sql(row.get(0))?;
        let mut message: UnifiedMessage = decode_row(row, 1)?;
        hydrate_source_aliases(&conn, &key, ordinal, &mut message)?;
        result.push(message);
    }
    Ok(result)
}

fn materialization_database() -> Result<Connection, String> {
    let conn = sql(Connection::open_in_memory())?;
    configure_connection(&conn)?;
    sql(conn.pragma_update(None, "temp_store", "MEMORY"))?;
    Ok(conn)
}

// Public exact-row consumers/predicates require every alias. One collapsed row
// can exceed the compact batch byte cap during this explicitly requested
// hydration; never truncate it. Internal compact queries leave the cohort on
// disk and selection never uses this native output vector as historical state.
fn hydrate_source_aliases(
    conn: &Connection,
    key: &[u8],
    ordinal: i64,
    message: &mut UnifiedMessage,
) -> Result<(), String> {
    let mut stmt = sql(
        conn.prepare("SELECT key FROM oc_aliases WHERE source=?1 AND ordinal=?2 ORDER BY position")
    )?;
    let mut rows = sql(stmt.query(params![key, ordinal]))?;
    while let Some(row) = sql(rows.next())? {
        let alias: String = sql(row.get(0))?;
        if message.dedup_key.as_ref() != Some(&alias) {
            message.dedup_aliases.push(alias);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCodeSelectionMode {
    Materialized,
    Report,
}

fn payload(message: &UnifiedMessage) -> Result<Vec<u8>, String> {
    encode(&(
        message.timestamp,
        message.duration_ms,
        &message.model_id,
        &message.provider_id,
        message.tokens.input,
        message.tokens.output,
        message.tokens.cache_read,
        message.tokens.cache_write,
        message.tokens.reasoning,
        &message.agent,
    ))
}

fn query_schema(conn: &Connection) -> Result<(), String> {
    sql(conn.execute_batch("DROP TABLE IF EXISTS temp.ocq_scope;
        DROP TABLE IF EXISTS temp.ocq_rows;
        DROP TABLE IF EXISTS temp.ocq_aliases;
        DROP TABLE IF EXISTS temp.ocq_hydration_aliases;
        DROP TABLE IF EXISTS temp.ocq_authority;
        DROP TABLE IF EXISTS temp.ocq_seen;
        DROP TABLE IF EXISTS temp.ocq_emitted;
        DROP TABLE IF EXISTS temp.ocq_deferred;
        DROP TABLE IF EXISTS temp.ocq_deferred_aliases;
        DROP TABLE IF EXISTS temp.ocq_connected;
        CREATE TEMP TABLE ocq_scope(source BLOB PRIMARY KEY,source_order INTEGER,kind INTEGER) WITHOUT ROWID;
        CREATE TEMP TABLE ocq_rows(id INTEGER PRIMARY KEY,source BLOB,ordinal INTEGER,source_order INTEGER,kind INTEGER,message BLOB,payload BLOB,provider INTEGER,timestamp INTEGER,primary_key TEXT);
        CREATE INDEX ocq_traversal ON ocq_rows(kind DESC,source_order,ordinal);
        CREATE TEMP TABLE ocq_aliases(id INTEGER,key TEXT,timestamp INTEGER,position INTEGER,PRIMARY KEY(id,key)) WITHOUT ROWID;
        CREATE INDEX ocq_alias_identity ON ocq_aliases(key,timestamp,id);
        CREATE TEMP TABLE ocq_hydration_aliases(id INTEGER,key TEXT,position INTEGER,PRIMARY KEY(id,key)) WITHOUT ROWID;
        CREATE TEMP TABLE ocq_authority(key TEXT,timestamp INTEGER,PRIMARY KEY(key,timestamp)) WITHOUT ROWID;
        CREATE TEMP TABLE ocq_seen(key TEXT,timestamp INTEGER,payload BLOB,PRIMARY KEY(key,timestamp,payload)) WITHOUT ROWID;
        CREATE TEMP TABLE ocq_emitted(key TEXT,timestamp INTEGER,PRIMARY KEY(key,timestamp)) WITHOUT ROWID;
        CREATE TEMP TABLE ocq_deferred(id INTEGER PRIMARY KEY,active INTEGER NOT NULL);
        CREATE TEMP TABLE ocq_deferred_aliases(registration INTEGER PRIMARY KEY,id INTEGER,key TEXT,timestamp INTEGER,position INTEGER,UNIQUE(id,key,timestamp));
        CREATE INDEX ocq_deferred_source ON ocq_deferred_aliases(key,timestamp,registration);
        CREATE TEMP TABLE ocq_connected(key TEXT,timestamp INTEGER,PRIMARY KEY(key,timestamp)) WITHOUT ROWID;"))
}

fn prepare_query(conn: &Connection, hydrate_aliases: bool) -> Result<(), String> {
    // Indexed usage facts were derived once at ingestion, not reconstructed by
    // a second native decode pass over unchanged historical rows.
    // Copy primary identity once; alias setup must not reread wide canonical rows.
    sql(conn.execute_batch("INSERT INTO ocq_rows(source,ordinal,source_order,kind,message,payload,provider,timestamp,primary_key)
        SELECT m.source,m.ordinal,s.source_order,s.kind,m.message,m.payload,m.provider,m.timestamp,m.primary_key
        FROM ocq_scope s CROSS JOIN oc_messages m ON m.source=s.source ORDER BY s.kind DESC,s.source_order,m.ordinal;"))?;
    sql(conn.execute_batch("INSERT INTO ocq_aliases SELECT id,primary_key,timestamp,-1 FROM ocq_rows WHERE primary_key IS NOT NULL AND primary_key!='';
        INSERT OR IGNORE INTO ocq_aliases SELECT r.id,a.key,r.timestamp,a.position FROM ocq_rows r JOIN oc_aliases a ON a.source=r.source AND a.ordinal=r.ordinal WHERE a.key!='';"))?;
    // Compact queries skip only the hydration mirror. Authority still needs
    // every alias below.
    if hydrate_aliases {
        sql(conn.execute_batch("INSERT INTO ocq_hydration_aliases SELECT r.id,a.key,a.position FROM ocq_rows r JOIN oc_aliases a ON a.source=r.source AND a.ordinal=r.ordinal;"))?;
    }
    sql(conn.execute_batch("INSERT OR IGNORE INTO ocq_authority SELECT a.key,a.timestamp FROM ocq_aliases a JOIN ocq_rows r ON r.id=a.id WHERE r.provider=1;"))?;
    // One indexed delta closure over shared (key,timestamp) aliases replaces
    // full-set fixed-point passes. UNION terminates cycles; every reached row
    // contributes all its aliases exactly like the old per-round expansion.
    sql(conn.execute_batch("INSERT OR IGNORE INTO ocq_authority WITH RECURSIVE reach(id) AS (
        SELECT DISTINCT b.id FROM ocq_aliases b JOIN ocq_authority h ON h.key=b.key AND h.timestamp=b.timestamp
        UNION
        SELECT a.id FROM ocq_aliases a JOIN ocq_aliases b ON b.key=a.key AND b.timestamp=a.timestamp JOIN reach r ON r.id=b.id
    ) SELECT DISTINCT a.key,a.timestamp FROM ocq_aliases a JOIN reach r ON r.id=a.id;"))?;
    Ok(())
}

struct DiskSelection<'a> {
    conn: &'a Connection,
}
impl DiskSelection<'_> {
    fn has(&self, table: &str, id: i64) -> Result<bool, String> {
        // Only the two internal selection tables reach this predicate; static
        // plans keep the cached set bounded across hot per-row checks.
        debug_assert!(table == "ocq_authority" || table == "ocq_emitted");
        let mut statement = if table == "ocq_authority" {
            sql(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM ocq_aliases a JOIN ocq_authority h ON h.key=a.key AND h.timestamp=a.timestamp WHERE a.id=?1)"))?
        } else {
            sql(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM ocq_aliases a JOIN ocq_emitted h ON h.key=a.key AND h.timestamp=a.timestamp WHERE a.id=?1)"))?
        };
        sql(statement.query_row([id], |row| row.get(0)))
    }
    fn mark_emitted(&self, id: i64, deferred: bool) -> Result<(), String> {
        let mut clear = sql(self.conn.prepare_cached("DELETE FROM ocq_connected"))?;
        sql(clear.execute([]))?;
        let mut seed = if deferred {
            sql(self.conn.prepare_cached(
                "INSERT OR IGNORE INTO ocq_connected SELECT key,timestamp FROM ocq_deferred_aliases WHERE id=?1",
            ))?
        } else {
            sql(self.conn.prepare_cached(
                "INSERT OR IGNORE INTO ocq_connected SELECT key,timestamp FROM ocq_aliases WHERE id=?1",
            ))?
        };
        sql(seed.execute([id]))?;
        // One indexed pass over deferred registrations replaces the full-set
        // fixed-point loop. UNION terminates cycles; inactive rows stay
        // reachable exactly like the old append-only expansion, so a
        // pathological alias cohort still never leaves disk.
        let mut closure = sql(self.conn.prepare_cached(
            "INSERT OR IGNORE INTO ocq_connected WITH RECURSIVE reach(id) AS (
                SELECT DISTINCT b.id FROM ocq_deferred_aliases b JOIN ocq_connected c ON c.key=b.key AND c.timestamp=b.timestamp
                UNION
                SELECT a.id FROM ocq_deferred_aliases a JOIN ocq_deferred_aliases b ON b.key=a.key AND b.timestamp=a.timestamp JOIN reach r ON r.id=b.id
            ) SELECT DISTINCT a.key,a.timestamp FROM ocq_deferred_aliases a JOIN reach r ON r.id=a.id",
        ))?;
        sql(closure.execute([]))?;
        let mut emitted = sql(self.conn.prepare_cached(
            "INSERT OR IGNORE INTO ocq_emitted SELECT key,timestamp FROM ocq_connected",
        ))?;
        sql(emitted.execute([]))?;
        Ok(())
    }
    fn find_deferred(&self, id: i64, exact: bool) -> Result<Option<i64>, String> {
        let mut statement = if exact {
            sql(self.conn.prepare_cached("SELECT d.id FROM ocq_aliases a JOIN ocq_deferred_aliases b ON b.key=a.key AND b.timestamp=a.timestamp JOIN ocq_deferred d ON d.id=b.id JOIN ocq_rows r ON r.id=d.id JOIN ocq_rows current ON current.id=a.id WHERE a.id=?1 AND d.active=1 AND r.payload=current.payload ORDER BY a.position,b.registration LIMIT 1"))?
        } else {
            sql(self.conn.prepare_cached("SELECT d.id FROM ocq_aliases a JOIN ocq_deferred_aliases b ON b.key=a.key AND b.timestamp=a.timestamp JOIN ocq_deferred d ON d.id=b.id JOIN ocq_rows r ON r.id=d.id JOIN ocq_rows current ON current.id=a.id WHERE a.id=?1 AND d.active=1 ORDER BY a.position,b.registration LIMIT 1"))?
        };
        sql(statement.query_row([id], |row| row.get(0)).optional())
    }
    fn index_deferred(&self, deferred: i64, incoming: i64) -> Result<(), String> {
        let mut statement = sql(self.conn.prepare_cached("INSERT OR IGNORE INTO ocq_deferred_aliases(id,key,timestamp,position) SELECT ?1,key,timestamp,position FROM ocq_aliases WHERE id=?2 ORDER BY position"))?;
        sql(statement.execute(params![deferred, incoming]))?;
        Ok(())
    }
    fn take_deferred(&self, id: i64) -> Result<(), String> {
        let mut statement = sql(self
            .conn
            .prepare_cached("UPDATE ocq_deferred SET active=0 WHERE id=?1"))?;
        sql(statement.execute([id]))?;
        self.mark_emitted(id, true)
    }
    fn select_sqlite(&self, id: i64, provider: bool) -> Result<bool, String> {
        let no_identity: bool = sql(sql(self
            .conn
            .prepare_cached("SELECT NOT EXISTS(SELECT 1 FROM ocq_aliases WHERE id=?1)"))?
        .query_row([id], |r| r.get(0)))?;
        if no_identity {
            return Ok(true);
        }
        let duplicate:bool=sql(sql(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM ocq_aliases a JOIN ocq_rows r ON r.id=a.id JOIN ocq_seen s ON s.key=a.key AND s.timestamp=a.timestamp AND s.payload=r.payload WHERE a.id=?1)"))?.query_row([id],|r|r.get(0)))?;
        sql(sql(self.conn.prepare_cached("INSERT OR IGNORE INTO ocq_seen SELECT a.key,a.timestamp,r.payload FROM ocq_aliases a JOIN ocq_rows r ON r.id=a.id WHERE a.id=?1"))?.execute([id]))?;
        if duplicate {
            if let Some(deferred) = self.find_deferred(id, true)? {
                self.index_deferred(deferred, id)?;
                if provider {
                    self.take_deferred(deferred)?;
                    self.mark_emitted(id, false)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            if self.has("ocq_emitted", id)? {
                self.mark_emitted(id, false)?;
            }
            return Ok(false);
        }
        if self.has("ocq_emitted", id)? {
            self.mark_emitted(id, false)?;
        }
        if self.has("ocq_authority", id)? && !provider {
            sql(sql(self
                .conn
                .prepare_cached("INSERT INTO ocq_deferred VALUES(?1,1)"))?
            .execute([id]))?;
            self.index_deferred(id, id)?;
            return Ok(false);
        }
        self.mark_emitted(id, false)?;
        Ok(true)
    }
    fn select_json(&self, id: i64, provider: bool, will_emit: bool) -> Result<bool, String> {
        let no_identity: bool = sql(sql(self
            .conn
            .prepare_cached("SELECT NOT EXISTS(SELECT 1 FROM ocq_aliases WHERE id=?1)"))?
        .query_row([id], |r| r.get(0)))?;
        if no_identity {
            return Ok(will_emit);
        }
        if self.has("ocq_authority", id)? {
            if !provider || !will_emit {
                return Ok(false);
            }
            let exact = self.find_deferred(id, true)?;
            if exact.is_none() && self.has("ocq_emitted", id)? {
                return Ok(false);
            }
            if let Some(deferred) = match exact {
                Some(value) => Some(value),
                None => self.find_deferred(id, false)?,
            } {
                self.take_deferred(deferred)?;
            }
            self.mark_emitted(id, false)?;
            return Ok(true);
        }
        if self.has("ocq_emitted", id)? {
            return Ok(false);
        }
        self.mark_emitted(id, false)?;
        Ok(will_emit)
    }
}

/// Registration participates in each source's atomic publication transaction.
/// The integration owner publishes the scope only after every source completes.
pub(crate) fn synchronize_opencode_sources(
    db_paths: &[PathBuf],
    json_paths: &[PathBuf],
    cache_root: &Path,
    register: &mut impl FnMut(&Connection, &Path, bool, usize, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut conn = open_database(cache_root)?;
    schema(&conn)?;
    for (kind, paths) in [(true, db_paths), (false, json_paths)] {
        for (order, path) in paths.iter().enumerate() {
            sync_source(&mut conn, path, kind, &mut |transaction, generation| {
                register(transaction, path, kind, order, generation)
            })?;
        }
    }
    Ok(())
}
/// Carry durable-known OpenCode identities the live scan no longer lists.
/// Stored (path, kind) identities absent from both discovered lists keep
/// their canonical generation referenced by the new scope. Archive orders
/// allocate after all live stages as current per-kind SQL max+1 (one query,
/// no per-row computation), so carried rows never collide with the live
/// SQLite PK space and live precedence holds. No cross-identity import: only
/// exact stored identities admitted by the captured scan carry.
pub(crate) fn carry_absent_opencode_sources(
    scan: &crate::scanner::ScanResult,
    db_paths: &[PathBuf],
    json_paths: &[PathBuf],
    cache_root: &Path,
    scope_generation: i64,
    register: &mut impl FnMut(&Connection, &Path, bool, usize, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    use std::collections::HashSet;
    let mut live: HashSet<(Vec<u8>, bool)> = HashSet::new();
    for path in db_paths.iter() {
        live.insert((source_key(path)?, true));
    }
    for path in json_paths.iter() {
        live.insert((source_key(path)?, false));
    }
    let conn = open_database(cache_root)?;
    schema(&conn)?;
    let mut statement =
        sql(conn.prepare("SELECT path,kind,generation FROM oc_sources ORDER BY kind DESC,path"))?;
    let mut rows = sql(statement.query([]))?;
    let mut carried: Vec<(Vec<u8>, bool, Vec<u8>)> = Vec::new();
    while let Some(row) = sql(rows.next())? {
        let path: Vec<u8> = sql(row.get(0))?;
        let kind: bool = sql(row.get(1))?;
        let generation: Vec<u8> = sql(row.get(2))?;
        if live.contains(&(path.clone(), kind)) {
            continue;
        }
        carried.push((path, kind, generation));
    }
    drop(rows);
    drop(statement);
    // Admit + decode first; orders come from the staged scope afterwards.
    let mut admitted: Vec<(PathBuf, bool, Vec<u8>)> = Vec::new();
    for (path, kind, generation) in carried {
        let decoded: CachedPath = bincode::options()
            .deserialize(&path)
            .map_err(|_| "OpenCode source path decoding failed")?;
        let decoded = decoded.into_path_buf();
        if !scan.admits_durable_source("opencode", &decoded) {
            continue;
        }
        // Deleted DBs carry; present-but-unreadable ones fail closed.
        match std::fs::symlink_metadata(&decoded) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => continue,
            Err(_) => return Err("OpenCode durable source discovery failed".into()),
        }
        admitted.push((decoded, kind, generation));
    }
    admitted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    // One max+1 lookup per kind against the staged scope rows registered so
    // far (live first, carried after): no per-row computation, no PK
    // collision with live DB orders.
    let mut sqlite_next = staged_opencode_next(&conn, scope_generation, true)?;
    let mut json_next = staged_opencode_next(&conn, scope_generation, false)?;
    for (decoded, kind, generation) in admitted {
        let order = if kind {
            let order = sqlite_next;
            sqlite_next += 1;
            order
        } else {
            let order = json_next;
            json_next += 1;
            order
        };
        register(&conn, &decoded, kind, order, &generation)?;
    }
    Ok(())
}

fn staged_opencode_next(
    conn: &Connection,
    scope_generation: i64,
    sqlite: bool,
) -> Result<usize, String> {
    // Another collector may stage a newer scope on a different connection.
    // Orders belong to this caller's generation, never the global maximum.
    let max: Option<i64> = sql(conn
        .query_row(
            "SELECT MAX(source_order) FROM sq_scope_sources WHERE kind=?1 AND scope_generation=?2",
            params![
                if sqlite {
                    "opencode-sqlite"
                } else {
                    "opencode-json"
                },
                scope_generation
            ],
            |row| row.get(0),
        )
        .optional())?
    .flatten();
    usize::try_from(
        max.unwrap_or(-1)
            .checked_add(1)
            .ok_or("OpenCode source order overflow")?,
    )
    .map_err(|_| "OpenCode source order is invalid".into())
}

/// Admit immutable staged generations, not their now-mutable native sources.
/// Each source was verified at ingestion; later writes belong to a later scope.
pub(crate) fn validate_opencode_scope_sources_on(
    conn: &Connection,
    scope_generation: i64,
) -> Result<(), String> {
    let exists: bool = sql(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sq_scope_generations WHERE id=?1)",
        [scope_generation],
        |row| row.get(0),
    ))?;
    if !exists {
        return Err("OpenCode staged scope is unavailable".into());
    }
    let mut statement=sql(conn.prepare(
        "SELECT s.path,s.kind='opencode-sqlite',g.path,g.kind,g.fingerprint,s.generation FROM sq_scope_sources s LEFT JOIN oc_generations g ON g.generation=s.generation
         WHERE s.scope_generation=?1 AND s.kind IN ('opencode-sqlite','opencode-json') ORDER BY s.kind DESC,s.source_order",
    ))?;
    let mut rows = sql(statement.query([scope_generation]))?;
    while let Some(row) = sql(rows.next())? {
        let path_bytes = sql(row.get_ref(0))?;
        let path_bytes = path_bytes
            .as_blob()
            .map_err(|_| "OpenCode source path has an invalid type")?;
        let sqlite: bool = sql(row.get(1))?;
        let stored_path = sql(row.get_ref(2))?;
        let stored_path = stored_path
            .as_blob()
            .map_err(|_| "OpenCode pinned source generation is unavailable")?;
        let stored_kind: bool = sql(row.get(3))?;
        if path_bytes != stored_path || sqlite != stored_kind {
            return Err("OpenCode pinned source descriptor does not match".into());
        }
        let stored = sql(row.get_ref(4))?;
        let stored = stored
            .as_blob()
            .map_err(|_| "OpenCode pinned source fingerprint is unavailable")?;
        let generation = sql(row.get_ref(5))?;
        let generation = generation
            .as_blob()
            .map_err(|_| "OpenCode pinned source generation is unavailable")?;
        if encode(&(path_bytes, stored, sqlite))?.as_slice() != generation {
            return Err("OpenCode pinned source fingerprint does not match".into());
        }
    }
    Ok(())
}

/// Durable canonical history is independent of source presence: an absent
/// native source keeps its current pointer and generation so later queries
/// still serve its facts. Only a complete, revalidated cutover of the same
/// scope (a superseding generation) retires a predecessor.
pub(crate) fn reconcile_missing_opencode_sources_on(
    conn: &Connection,
    previous_scope_generation: i64,
    current_scope_generation: i64,
) -> Result<(), String> {
    if conn.is_autocommit() {
        return Err("OpenCode reconciliation requires a scope transaction".into());
    }
    if previous_scope_generation == 0 {
        return Ok(());
    }
    let same_scope: bool = sql(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sq_scope_generations p JOIN sq_scope_generations n ON n.scope_id=p.scope_id WHERE p.id=?1 AND n.id=?2 AND p.complete=1)",
        params![previous_scope_generation,current_scope_generation],
        |row| row.get(0),
    ))?;
    if !same_scope {
        return Err("OpenCode reconciliation scope does not match".into());
    }
    let mut statement=sql(conn.prepare(
        "SELECT DISTINCT c.path,c.generation,c.kind FROM oc_sources c JOIN sq_scope_sources p ON p.path=c.path AND p.kind=CASE WHEN c.kind=1 THEN 'opencode-sqlite' ELSE 'opencode-json' END
         WHERE p.scope_generation=?1 AND NOT EXISTS(SELECT 1 FROM sq_scope_sources n WHERE n.scope_generation=?2 AND n.path=c.path AND n.kind=p.kind) ORDER BY c.path",
    ))?;
    let mut rows =
        sql(statement.query(params![previous_scope_generation, current_scope_generation]))?;
    while let Some(row) = sql(rows.next())? {
        let path_bytes = sql(row.get_ref(0))?;
        let path_bytes = path_bytes
            .as_blob()
            .map_err(|_| "OpenCode source path has an invalid type")?;
        let path: CachedPath = bincode::options()
            .deserialize(path_bytes)
            .map_err(|_| "OpenCode source path decoding failed")?;
        match fs::metadata(path.into_path_buf()) {
            Ok(_) => {}
            // Absent sources keep their pointer: durable queries must serve
            // their canonical facts after deletion or DB disappearance.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("OpenCode source discovery failed".into()),
        }
    }
    Ok(())
}

/// Durable canonical OpenCode facts survive source absence, DB disappearance,
/// and scope eviction. Only a superseding newer generation of the same
/// (path, kind) source identity may retire its predecessor; unreferenced
/// intermediates are dropped while the newest generation per identity and
/// every scope-pinned generation stay queryable.
pub(crate) fn discard_unreferenced_opencode_generations(conn: &Connection) -> Result<(), String> {
    sql(conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS oc_gc(generation BLOB PRIMARY KEY) WITHOUT ROWID;
        DELETE FROM oc_gc;
        INSERT INTO oc_gc SELECT g.generation FROM oc_generations g WHERE
            g.generation NOT IN (SELECT max(g2.generation) FROM oc_generations g2 WHERE g2.path=g.path AND g2.kind=g.kind) AND
            NOT EXISTS(SELECT 1 FROM oc_sources s WHERE s.generation=g.generation) AND
            NOT EXISTS(SELECT 1 FROM sq_scope_sources s WHERE s.generation=g.generation AND s.kind IN ('opencode-sqlite','opencode-json'));
        DELETE FROM oc_rows WHERE source IN (SELECT generation FROM oc_gc);
        DELETE FROM oc_messages WHERE source IN (SELECT generation FROM oc_gc);
        DELETE FROM oc_aliases WHERE source IN (SELECT generation FROM oc_gc);
        DELETE FROM oc_generations WHERE generation IN (SELECT generation FROM oc_gc);
        DELETE FROM oc_gc;"))
}

#[cfg(test)]
fn visit_opencode_snapshot<F, S>(
    cache_root: &Path,
    scope_generation: i64,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    let mut conn = open_database(cache_root)?;
    let transaction = sql(conn.transaction())?;
    visit_opencode_snapshot_on(
        &transaction,
        scope_generation,
        mode,
        hydrate_aliases,
        filter,
        sink,
    )
}

#[cfg(test)]
/// Pure query on the caller's canonical read transaction. Repeated inference
/// and aggregate passes share the same snapshot, including during concurrent GC.
pub(crate) fn visit_opencode_snapshot_on<F, S>(
    transaction: &Connection,
    scope_generation: i64,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    prepare_opencode_snapshot_on(transaction, scope_generation, hydrate_aliases)?;
    visit_prepared_opencode_snapshot_on(transaction, mode, hydrate_aliases, &|_| {}, filter, sink)
}

/// Transfer immutable facts and aliases while the scope transaction is pinned.
/// No permanent table is read after this returns.
pub(crate) fn prepare_opencode_snapshot_on(
    transaction: &Connection,
    scope_generation: i64,
    hydrate_aliases: bool,
) -> Result<(), String> {
    if transaction.is_autocommit() {
        return Err("OpenCode snapshot query requires a pinned transaction".into());
    }
    let complete: bool = sql(transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sq_scope_generations WHERE id=?1 AND complete=1)",
        [scope_generation],
        |row| row.get(0),
    ))?;
    if !complete {
        return Err("OpenCode scope snapshot is unavailable".into());
    }
    let valid:bool=sql(transaction.query_row("SELECT NOT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN oc_generations g ON g.generation=s.generation WHERE s.scope_generation=?1 AND s.kind IN ('opencode-sqlite','opencode-json') AND (g.generation IS NULL OR g.path!=s.path OR g.kind!=(s.kind='opencode-sqlite')))",[scope_generation],|row|row.get(0)))?;
    if !valid {
        return Err("OpenCode pinned source generation is unavailable".into());
    }
    query_schema(transaction)?;
    sql(transaction.execute("INSERT OR IGNORE INTO ocq_scope SELECT generation,source_order,kind='opencode-sqlite' FROM sq_scope_sources WHERE scope_generation=?1 AND kind IN ('opencode-sqlite','opencode-json') ORDER BY kind DESC,source_order",[scope_generation]))?;
    prepare_query(transaction, hydrate_aliases)
}

#[cfg(test)]
fn visit_opencode_messages<F, S>(
    db_paths: &[PathBuf],
    json_paths: &[PathBuf],
    cache_root: &Path,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    let mut conn = open_database(cache_root)?;
    schema(&conn)?;
    query_schema(&conn)?;
    for (kind, paths) in [(true, db_paths), (false, json_paths)] {
        for (order, path) in paths.iter().enumerate() {
            sync_source(&mut conn, path, kind, &mut |transaction, generation| {
                sql(transaction.execute(
                    "INSERT OR IGNORE INTO ocq_scope VALUES(?1,?2,?3)",
                    params![generation, order as i64, kind],
                ))?;
                Ok(())
            })?;
        }
    }
    let transaction = sql(conn.transaction())?;
    visit_selected(&transaction, mode, hydrate_aliases, filter, sink)
}

#[cfg(test)]
fn visit_selected<F, S>(
    transaction: &Connection,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    prepare_query(transaction, hydrate_aliases)?;
    visit_prepared_opencode_snapshot_on(transaction, mode, hydrate_aliases, &|_| {}, filter, sink)
}

pub(crate) fn visit_opencode_query_on<F, S>(
    connection: &Connection,
    indexed: bool,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    prepare: &impl Fn(&mut UnifiedMessage),
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    if indexed {
        visit_indexed_opencode_on(connection, hydrate_aliases, prepare, filter, sink)
    } else {
        visit_prepared_opencode_snapshot_on(
            connection,
            mode,
            hydrate_aliases,
            prepare,
            filter,
            sink,
        )
    }
}

/// Materialized LocalClients ownership is independent of all later predicates.
/// Keep only winner references; composed source facts/aliases stay in their
/// canonical immutable generation rather than another payload mirror.
pub(crate) fn persist_local_opencode_decisions(
    connection: &Connection,
    generation: i64,
) -> Result<(), String> {
    sql(connection.execute_batch("DELETE FROM ocq_seen; DELETE FROM ocq_emitted; DELETE FROM ocq_deferred; DELETE FROM ocq_deferred_aliases; DELETE FROM ocq_connected;"))?;
    let selection = DiskSelection { conn: connection };
    let record = |id: i64| -> Result<(), String> {
        // The rank subquery is already a cheap PK-seek aggregate that runs once
        // per emitted winner; only the statement plan itself is worth caching.
        let mut statement = sql(connection.prepare_cached("INSERT INTO sq_decisions(scope_generation,rank,lane,timestamp,oc_source,oc_ordinal)
            SELECT ?1,(SELECT COALESCE(MAX(rank),0)+1 FROM sq_decisions WHERE scope_generation=?1),'opencode',timestamp,source,ordinal FROM ocq_rows WHERE id=?2"))?;
        sql(statement.execute(params![generation, id]))?;
        Ok(())
    };
    let mut statement = sql(connection
        .prepare("SELECT id,kind,provider FROM ocq_rows ORDER BY kind DESC,source_order,ordinal"))?;
    let mut rows = sql(statement.query([]))?;
    while let Some(row) = sql(rows.next())? {
        let id: i64 = sql(row.get(0))?;
        let sqlite: bool = sql(row.get(1))?;
        let provider: bool = sql(row.get(2))?;
        let emit = if sqlite {
            selection.select_sqlite(id, provider)?
        } else {
            selection.select_json(id, provider, true)?
        };
        if emit {
            record(id)?;
        }
    }
    drop(rows);
    drop(statement);
    let mut statement =
        sql(connection.prepare("SELECT id FROM ocq_deferred WHERE active=1 ORDER BY id"))?;
    let mut rows = sql(statement.query([]))?;
    while let Some(row) = sql(rows.next())? {
        record(sql(row.get(0))?)?;
    }
    Ok(())
}

/// Reuse a previous complete LocalClients OpenCode lane when its exact
/// dependencies match. Only `opencode-sqlite`/`opencode-json` rows are
/// compared (kind, path, generation, source order, workspace references, both
/// directions) and only `lane='opencode'` decisions are copied with their
/// rank and references. Empty matches empty only. Returns false on any
/// mismatch so the caller keeps the existing prepare+persist path. Runs
/// inside the publication transaction before GC; the current scope was
/// already validated.
pub(crate) fn reuse_local_opencode_decisions(
    connection: &Connection,
    current_scope_generation: i64,
) -> Result<bool, String> {
    let selection: Option<i64> = sql(connection
        .query_row(
            "SELECT selection FROM sq_scope_generations WHERE id=?1",
            [current_scope_generation],
            |row| row.get(0),
        )
        .optional())?;
    let Some(selection) = selection else {
        return Err("usage scope generation is unavailable".into());
    };
    let mut statement = sql(connection.prepare_cached(
        "SELECT g.id FROM sq_scope_generations g JOIN sq_indexes i ON i.scope_generation=g.id AND i.version=1 WHERE g.id!=?1 AND g.complete=1 AND g.selection=?2 ORDER BY g.id DESC",
    ))?;
    let mut candidates = sql(statement.query(params![current_scope_generation, selection]))?;
    while let Some(candidate) = sql(candidates.next())? {
        let previous: i64 = sql(candidate.get(0))?;
        let exact: bool = sql(connection.query_row(
            "SELECT NOT EXISTS(SELECT kind,path,generation,source_order,workspace,workspace_label FROM sq_scope_sources WHERE scope_generation=?1 AND kind IN ('opencode-sqlite','opencode-json') EXCEPT SELECT kind,path,generation,source_order,workspace,workspace_label FROM sq_scope_sources WHERE scope_generation=?2 AND kind IN ('opencode-sqlite','opencode-json')) AND NOT EXISTS(SELECT kind,path,generation,source_order,workspace,workspace_label FROM sq_scope_sources WHERE scope_generation=?2 AND kind IN ('opencode-sqlite','opencode-json') EXCEPT SELECT kind,path,generation,source_order,workspace,workspace_label FROM sq_scope_sources WHERE scope_generation=?1 AND kind IN ('opencode-sqlite','opencode-json'))",
            params![current_scope_generation, previous],
            |row| row.get(0),
        ))?;
        if !exact {
            continue;
        }
        // Exact descriptors are not enough if the referenced facts are gone;
        // fall back to a full rebuild rather than copy dangling references.
        let dangling: bool = sql(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sq_decisions d LEFT JOIN oc_messages m ON m.source=d.oc_source AND m.ordinal=d.oc_ordinal WHERE d.scope_generation=?1 AND d.lane='opencode' AND d.oc_source IS NOT NULL AND m.source IS NULL)",
            [previous],
            |row| row.get(0),
        ))?;
        if dangling {
            continue;
        }
        sql(connection.execute(
            "INSERT INTO sq_decisions(scope_generation,rank,lane,timestamp,cache_source_id,cache_generation,cache_ordinal,oc_source,oc_ordinal,retained,global_storage,workspace,workspace_label,model,headless) SELECT ?1,rank,lane,timestamp,cache_source_id,cache_generation,cache_ordinal,oc_source,oc_ordinal,retained,global_storage,workspace,workspace_label,model,headless FROM sq_decisions WHERE scope_generation=?2 AND lane='opencode' ORDER BY rank",
            params![current_scope_generation, previous],
        ))?;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn prepare_indexed_opencode_on(
    connection: &Connection,
    generation: i64,
    query: crate::store_query::UsageQuery,
) -> Result<(), String> {
    query_schema(connection)?;
    let mut gate = String::from("d.scope_generation=?1 AND d.oc_source IS NOT NULL");
    let mut values: Vec<&dyn rusqlite::ToSql> = vec![&generation];
    if let Some(from) = query.from_ms.as_ref() {
        gate.push_str(" AND d.timestamp>=?");
        values.push(from);
    }
    if let Some(until) = query.until_ms.as_ref() {
        gate.push_str(" AND d.timestamp<?");
        values.push(until);
    }
    sql(connection.execute(&format!("INSERT INTO ocq_rows(id,source,ordinal,source_order,kind,message,timestamp)
        SELECT d.rank,d.oc_source,d.oc_ordinal,d.rank,1,m.message,d.timestamp
        FROM sq_decisions d INDEXED BY sq_decision_time LEFT JOIN oc_messages m ON m.source=d.oc_source AND m.ordinal=d.oc_ordinal WHERE {gate} ORDER BY d.rank"), values.as_slice()))?;
    let missing: bool = sql(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM ocq_rows WHERE message IS NULL)",
        [],
        |r| r.get(0),
    ))?;
    if missing {
        return Err("OpenCode decision references an unavailable generation".into());
    }
    if query.hydrate_opencode_aliases {
        sql(connection.execute_batch("INSERT INTO ocq_hydration_aliases SELECT r.id,a.key,a.position FROM ocq_rows r CROSS JOIN oc_aliases a ON a.source=r.source AND a.ordinal=r.ordinal;"))?;
    }
    Ok(())
}

pub(crate) fn visit_indexed_opencode_on<F, S>(
    connection: &Connection,
    hydrate_aliases: bool,
    prepare: &impl Fn(&mut UnifiedMessage),
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    let mut statement = sql(connection.prepare("SELECT id,message FROM ocq_rows ORDER BY id"))?;
    let mut rows = sql(statement.query([]))?;
    while let Some(row) = sql(rows.next())? {
        let mut message: UnifiedMessage = decode_row(row, 1)?;
        if hydrate_aliases {
            hydrate_prepared_aliases(connection, sql(row.get(0))?, &mut message)?;
        }
        prepare(&mut message);
        if filter(&message) {
            sink(&message);
        }
    }
    Ok(())
}

fn hydrate_prepared_aliases(
    connection: &Connection,
    id: i64,
    message: &mut UnifiedMessage,
) -> Result<(), String> {
    let mut statement =
        sql(connection
            .prepare("SELECT key FROM ocq_hydration_aliases WHERE id=?1 ORDER BY position"))?;
    let mut rows = sql(statement.query([id]))?;
    while let Some(row) = sql(rows.next())? {
        let alias: String = sql(row.get(0))?;
        if message.dedup_key.as_ref() != Some(&alias) {
            message.dedup_aliases.push(alias);
        }
    }
    Ok(())
}

/// Filter and fold only connection-local owned facts; callbacks may synchronize
/// newer canonical generations without blocking on this reader.
/// Batch scratch-only arbitration in a savepoint: one commit for the whole
/// fold instead of one per row, rolled back on error.
pub(crate) fn visit_prepared_opencode_snapshot_on<F, S>(
    transaction: &Connection,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    prepare: &impl Fn(&mut UnifiedMessage),
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    // Scratch-only savepoint batches arbitration into one commit instead of one
    // per row, rolled back on error. A savepoint never takes the canonical
    // write lock, so public callbacks can still publish mid-fold, and
    // publication (which already holds its own write transaction) never sees
    // an unconditional begin from shared arbitration.
    sql(transaction.execute_batch("SAVEPOINT ocq_scratch"))?;
    let result = visit_prepared_opencode_snapshot_inner(
        transaction,
        mode,
        hydrate_aliases,
        prepare,
        filter,
        sink,
    );
    if result.is_err() {
        let _ = transaction.execute_batch("ROLLBACK TO SAVEPOINT ocq_scratch; RELEASE ocq_scratch");
    } else {
        sql(transaction.execute_batch("RELEASE ocq_scratch"))?;
    }
    result
}

pub(crate) fn visit_prepared_opencode_snapshot_inner<F, S>(
    transaction: &Connection,
    mode: OpenCodeSelectionMode,
    hydrate_aliases: bool,
    prepare: &impl Fn(&mut UnifiedMessage),
    filter: &F,
    sink: &mut S,
) -> Result<(), String>
where
    F: Fn(&UnifiedMessage) -> bool,
    S: FnMut(&UnifiedMessage),
{
    sql(transaction.execute_batch("DELETE FROM ocq_seen; DELETE FROM ocq_emitted; DELETE FROM ocq_deferred; DELETE FROM ocq_deferred_aliases; DELETE FROM ocq_connected;"))?;
    let selection = DiskSelection { conn: transaction };
    let mut stmt = sql(transaction.prepare(
        "SELECT id,kind,provider,message FROM ocq_rows ORDER BY kind DESC,source_order,ordinal",
    ))?;
    let mut rows = sql(stmt.query([]))?;
    while let Some(row) = sql(rows.next())? {
        let id: i64 = sql(row.get(0))?;
        let sqlite: bool = sql(row.get(1))?;
        let provider: bool = sql(row.get(2))?;
        let mut message: UnifiedMessage = decode_row(row, 3)?;
        if hydrate_aliases {
            // Public opaque predicates observe the original candidate fields,
            // including JSON eligibility before report fallback arbitration.
            hydrate_prepared_aliases(transaction, id, &mut message)?;
        }
        prepare(&mut message);
        let passes = filter(&message);
        let emit = if sqlite {
            selection.select_sqlite(id, provider)?
        } else {
            selection.select_json(
                id,
                provider,
                mode != OpenCodeSelectionMode::Report || passes,
            )?
        };
        if emit && passes {
            sink(&message);
        }
    }
    drop(rows);
    drop(stmt);
    let mut stmt=sql(transaction.prepare("SELECT r.message,r.id FROM ocq_deferred d JOIN ocq_rows r ON r.id=d.id WHERE d.active=1 ORDER BY d.id"))?;
    let mut rows = sql(stmt.query([]))?;
    while let Some(row) = sql(rows.next())? {
        let mut message: UnifiedMessage = decode_row(row, 0)?;
        if hydrate_aliases {
            let id: i64 = sql(row.get(1))?;
            hydrate_prepared_aliases(transaction, id, &mut message)?;
        }
        prepare(&mut message);
        if filter(&message) {
            sink(&message);
        }
    }
    Ok(())
}

const REPORT_STATE_BYTE_LIMIT: usize = 1024 * 1024;

/// One serialization pass with a hard byte ceiling. Small aggregates do not
/// reserve the full ceiling, and oversized states never allocate beyond it.
#[derive(Default)]
struct ReportStateBuffer {
    bytes: Vec<u8>,
    oversized: bool,
}

impl std::io::Write for ReportStateBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > REPORT_STATE_BYTE_LIMIT - self.bytes.len() {
            self.oversized = true;
            return Err(std::io::Error::other("report checkpoint byte limit"));
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(128)
                .clamp(needed, REPORT_STATE_BYTE_LIMIT);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_report_state<T: Serialize>(state: &T) -> Result<Option<Vec<u8>>, String> {
    let mut output = ReportStateBuffer::default();
    match bincode::serialize_into(&mut output, state) {
        Ok(()) => Ok(Some(output.bytes)),
        Err(_) if output.oversized => Ok(None),
        Err(_) => Err("usage report checkpoint encoding failed".into()),
    }
}

/// Compact sequential fold states, never partial floating-point sums or facts.
/// Membership is checked before decoding a historical prefix. Checkpoints are
/// disposable, but their admission and storage errors still fail the query.
pub(crate) struct ReportCheckpoint {
    key: [u8; 32],
    restored: std::cell::RefCell<Option<Vec<u8>>>,
    pub(crate) resume_rows: i64,
}

impl ReportCheckpoint {
    pub(crate) fn disabled(key: [u8; 32]) -> Self {
        Self {
            key,
            restored: std::cell::RefCell::new(None),
            resume_rows: 0,
        }
    }

    pub(crate) fn open(connection: &Connection, key: [u8; 32]) -> Result<Self, String> {
        sql(connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS report_checkpoints(
                id INTEGER PRIMARY KEY,
                query_key BLOB NOT NULL,
                row_count INTEGER NOT NULL,
                membership BLOB NOT NULL,
                state BLOB NOT NULL,
                state_digest BLOB NOT NULL,
                UNIQUE(query_key,row_count,membership));
             CREATE TEMP TABLE report_checkpoint_staged(
                row_count INTEGER PRIMARY KEY,membership BLOB NOT NULL,
                state BLOB NOT NULL,state_digest BLOB NOT NULL);",
        ))?;
        let state: Option<(i64, Vec<u8>, Vec<u8>)> = sql(connection
            .query_row(
                "SELECT c.row_count,c.state,c.state_digest FROM report_checkpoints c
             JOIN sq_fold_membership m ON m.position=c.row_count AND m.membership=c.membership
             WHERE c.query_key=?1 AND length(c.state)<=1048576
             ORDER BY c.row_count DESC LIMIT 1",
                [key.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional())?;
        if let Some((position, state, digest)) = state {
            use sha2::{Digest, Sha256};
            if Sha256::digest(&state)[..] != digest[..] {
                return Err("usage report checkpoint checksum mismatch".into());
            }
            Ok(Self {
                key,
                restored: std::cell::RefCell::new(Some(state)),
                resume_rows: position,
            })
        } else {
            Ok(Self::disabled(key))
        }
    }

    pub(crate) fn has_restored(&self) -> bool {
        self.restored.borrow().is_some()
    }

    pub(crate) fn restore<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        let bytes = self.restored.borrow_mut().take();
        bytes
            .map(|bytes| {
                bincode::DefaultOptions::new()
                    .with_fixint_encoding()
                    .with_limit(1024 * 1024)
                    .reject_trailing_bytes()
                    .deserialize(&bytes)
                    .map_err(|_| "usage report checkpoint decoding failed".to_string())
            })
            .transpose()
    }

    pub(crate) fn save<T: Serialize>(
        &self,
        connection: &Connection,
        position: i64,
        state: &T,
    ) -> Result<(), String> {
        // Do not walk the aggregate once to size it, then twice more through
        // bincode::serialize. The capped writer declines optional retention
        // without truncating the report or eagerly allocating 1 MiB.
        let Some(state) = encode_report_state(state)? else {
            return Ok(());
        };
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&state);
        sql(connection.execute(
            "INSERT OR REPLACE INTO report_checkpoint_staged(row_count,membership,state,state_digest)
             SELECT position,membership,?2,?3 FROM sq_fold_membership WHERE position=?1",
            params![position, state, digest.as_slice()],
        ))?;
        sql(connection.execute_batch(
            "DELETE FROM report_checkpoint_staged WHERE row_count NOT IN
                 (SELECT row_count FROM report_checkpoint_staged ORDER BY row_count DESC LIMIT 128);
             DELETE FROM report_checkpoint_staged WHERE row_count IN (
                 SELECT row_count FROM (
                     SELECT row_count,SUM(length(state)+length(membership)+length(state_digest)+48)
                         OVER (ORDER BY row_count DESC) AS retained_bytes
                     FROM report_checkpoint_staged
                 ) WHERE retained_bytes>16777216);",
        ))?;
        Ok(())
    }

    /// Publish optional states only after the entire trusted fold succeeds.
    /// Temporary snapshots spill to disk, have the same caps as final entries,
    /// and disappear on an error without touching canonical source facts.
    pub(crate) fn finish(&self, connection: &Connection) -> Result<(), String> {
        let pending: bool = sql(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM report_checkpoint_staged)",
            [],
            |row| row.get(0),
        ))?;
        if !pending {
            return Ok(());
        }
        let transaction = sql(connection.unchecked_transaction())?;
        sql(transaction.execute(
            "INSERT OR REPLACE INTO report_checkpoints(query_key,row_count,membership,state,state_digest)
             SELECT ?1,row_count,membership,state,state_digest FROM report_checkpoint_staged
             ORDER BY row_count",
            [self.key.as_slice()],
        ))?;
        sql(transaction.execute_batch(
            "DELETE FROM report_checkpoints WHERE id NOT IN
                 (SELECT id FROM report_checkpoints ORDER BY id DESC LIMIT 128);
             DELETE FROM report_checkpoints WHERE id IN (
                 SELECT id FROM (
                     SELECT id,SUM(length(state)+length(query_key)+length(membership)+length(state_digest)+32)
                         OVER (ORDER BY id DESC) AS retained_bytes
                     FROM report_checkpoints
                 ) WHERE retained_bytes>16777216);",
        ))?;
        sql(transaction.commit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tempfile::TempDir;

    type TransferHook = Box<dyn FnOnce(&Path)>;
    thread_local! {
        static TRANSFER_HOOK: RefCell<Option<TransferHook>> = const { RefCell::new(None) };
    }

    pub(super) fn run_transfer_hook(path: &Path) {
        let hook = TRANSFER_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook(path);
        }
    }

    fn usage(id: Option<&str>, input: i64, cost: f64, created: f64) -> serde_json::Value {
        let mut value = serde_json::json!({
            "role":"assistant", "modelID":"gpt-4o", "providerID":"openai",
            "sessionID":"session", "cost":cost,
            "tokens":{"input":input,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},
            "time":{"created":created,"completed":created+10.0}
        });
        if let Some(id) = id {
            value["id"] = id.into();
        }
        value
    }

    fn fixture(path: &Path, wal: bool) -> Connection {
        let conn = Connection::open(path).unwrap();
        if wal {
            conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        }
        conn.execute_batch(
            "CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT);
            CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT);
            INSERT INTO session VALUES('session','/workspace/current');",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, row: &str, value: serde_json::Value) {
        conn.execute(
            "INSERT OR REPLACE INTO message VALUES(?1,'session',?2)",
            params![row, value.to_string()],
        )
        .unwrap();
    }

    fn collect(
        paths: &[PathBuf],
        json: &[PathBuf],
        cache: &Path,
        mode: OpenCodeSelectionMode,
        filter: &impl Fn(&UnifiedMessage) -> bool,
    ) -> Result<Vec<UnifiedMessage>, String> {
        let mut result = Vec::new();
        visit_opencode_messages(paths, json, cache, mode, false, filter, &mut |m| {
            result.push(m.clone())
        })?;
        Ok(result)
    }

    fn encoded_source(conn: &Connection, path: &Path) -> Vec<u8> {
        conn.query_row(
            "SELECT fingerprint FROM oc_sources WHERE path=?1",
            [source_key(path).unwrap()],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn cold_warm_wal_update_and_complete_delete_are_atomic() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, true);
        insert(
            &source,
            "row-a",
            usage(Some("embedded-a"), 3, 0.25, 1_700_000_000_000.25),
        );
        insert(
            &source,
            "row-b",
            usage(Some("embedded-b"), 4, 0.0, 1_700_000_000_001.0),
        );
        let cache = root.path().join("cache");
        let paths = [path.clone()];
        let first = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(first.len(), 2);
        let probe = open_database(&cache).unwrap();
        let revision: i64 = probe
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let fingerprint = encoded_source(&probe, &path);
        let warm = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(encode(&first).unwrap(), encode(&warm).unwrap());
        assert_eq!(
            revision,
            probe
                .pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0))
                .unwrap()
        );
        assert_eq!(fingerprint, encoded_source(&probe, &path));
        // No timestamp/row-id advance: invalidation must observe the WAL itself.
        insert(
            &source,
            "row-a",
            usage(Some("embedded-a"), 31, 0.25, 1_700_000_000_000.25),
        );
        source
            .execute("DELETE FROM message WHERE id='row-b'", [])
            .unwrap();
        let updated = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(
            updated
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            35
        );
        assert_eq!(updated[0].tokens.input, 31);
        assert_ne!(fingerprint, encoded_source(&probe, &path));
        // Durable facts survive row deletion: an emptied live source carries
        // both canonical rows forward (updated 31 + removed 4).
        source.execute("DELETE FROM message", []).unwrap();
        let retained = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(retained.len(), 2);
        assert_eq!(
            retained
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            35
        );
    }

    #[test]
    fn report_filter_preserves_sqlite_fallback_but_materialized_does_not() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, true);
        let mut sqlite = usage(Some("shared"), 3, 0.0, 1_700_000_000_000.0);
        sqlite["modelID"] = "plain-model".into();
        insert(&source, "row", sqlite);
        let json = root.path().join("shared.json");
        let mut reported = usage(Some("shared"), 3, 0.75, 1_700_000_000_000.0);
        reported["modelID"] = "filtered-model".into();
        fs::write(&json, reported.to_string()).unwrap();
        let paths = [path];
        let files = [json];
        let cache = root.path().join("cache");
        let filter = |message: &UnifiedMessage| message.model_id == "plain-model";
        let report = collect(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Report,
            &filter,
        )
        .unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].model_id, "plain-model");
        assert!(collect(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Materialized,
            &filter
        )
        .unwrap()
        .is_empty());
        let selected = collect(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].cost, 0.75);
        assert_eq!(selected[0].cost_source, CostSource::ProviderReported);
        assert_eq!(
            encode(&selected).unwrap(),
            encode(
                &collect(
                    &paths,
                    &files,
                    &cache,
                    OpenCodeSelectionMode::Materialized,
                    &|_| true
                )
                .unwrap()
            )
            .unwrap()
        );
    }

    #[test]
    fn partial_read_and_missing_path_preserve_previous_successful_generation() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        insert(&source, "a", usage(Some("a"), 3, 0.25, 1_700_000_000_000.0));
        insert(&source, "z", usage(Some("z"), 4, 0.0, 1_700_000_000_001.0));
        let cache = root.path().join("cache");
        let paths = [path.clone()];
        collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        let probe = open_database(&cache).unwrap();
        let old = encoded_source(&probe, &path);
        insert(
            &source,
            "a",
            usage(Some("a"), 300, 0.25, 1_700_000_000_000.0),
        );
        source
            .execute("UPDATE message SET data='invalid json' WHERE id='z'", [])
            .unwrap();
        let mut emitted = 0;
        assert!(visit_opencode_messages(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |_| emitted += 1
        )
        .is_err());
        assert_eq!(emitted, 0);
        assert_eq!(old, encoded_source(&probe, &path));
        let blob:Vec<u8>=probe.query_row("SELECT message FROM oc_messages WHERE source=(SELECT generation FROM oc_sources WHERE path=?1) AND ordinal=0",[source_key(&path).unwrap()],|r|r.get(0)).unwrap();
        assert_eq!(decode::<UnifiedMessage>(&blob).unwrap().tokens.input, 3);
        drop(source);
        fs::rename(&path, root.path().join("preserved.db")).unwrap();
        // Durable facts survive the rename: the absent source carries its
        // last-good generation forward instead of failing the scope.
        let carried = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(carried.len(), 2);
        assert_eq!(old, encoded_source(&probe, &path));
        // An empty exact scope is not a failed source read.
        assert!(collect(
            &[],
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn snapshot_race_retries_boundedly_and_keeps_old_generation() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        insert(&source, "a", usage(Some("a"), 3, 0.0, 1_700_000_000_000.0));
        let cache = root.path().join("cache");
        let paths = [path.clone()];
        collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        let probe = open_database(&cache).unwrap();
        let previous = encoded_source(&probe, &path);
        insert(&source, "a", usage(Some("a"), 4, 0.0, 1_700_000_000_000.0));
        TRANSFER_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|path| {
                let writer = Connection::open(path).unwrap();
                // Rollback journal requires the source reader to have been released.
                insert(&writer, "a", usage(Some("a"), 5, 0.0, 1_700_000_000_000.0));
                TRANSFER_HOOK.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(|path| {
                        let writer = Connection::open(path).unwrap();
                        insert(&writer, "a", usage(Some("a"), 6, 0.0, 1_700_000_000_000.0));
                    }))
                });
            }))
        });
        let error = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap_err();
        assert!(error.contains("changed during snapshot"));
        assert_eq!(previous, encoded_source(&probe, &path));
        let stable = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        assert_eq!(stable[0].tokens.input, 6);
    }

    fn assert_disk_selection_matches_original(
        sqlite: Vec<UnifiedMessage>,
        json: Vec<UnifiedMessage>,
    ) -> Vec<UnifiedMessage> {
        use crate::{opencode_authoritative_sources, opencode_identity_group, OpenCodeSelection};
        let root = TempDir::new().unwrap();
        let mut conn = open_database(&root.path().join("cache")).unwrap();
        schema(&conn).unwrap();
        let key = source_key(&root.path().join("source.db")).unwrap();
        let json_key = source_key(&root.path().join("authority.json")).unwrap();
        let transaction = conn.transaction().unwrap();
        for (source, kind, messages) in [(&key, true, &sqlite[..]), (&json_key, false, &json[..])] {
            transaction
                .execute(
                    "INSERT INTO oc_sources VALUES(?1,x'00',?2,?1)",
                    params![source, kind],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO oc_generations VALUES(?1,?1,x'00',?2)",
                    params![source, kind],
                )
                .unwrap();
            for (ordinal, message) in messages.iter().enumerate() {
                let mut compact = message.clone();
                compact.dedup_aliases.clear();
                transaction
                    .execute(
                        "INSERT INTO oc_messages VALUES(?1,?2,x'00',NULL,0,0,?3,?4,?5,?6,?7)",
                        params![
                            source,
                            ordinal as i64,
                            encode(&compact).unwrap(),
                            payload(&compact).unwrap(),
                            compact.timestamp,
                            compact.cost_source == CostSource::ProviderReported,
                            compact.dedup_key
                        ],
                    )
                    .unwrap();
                for (position, alias) in message
                    .dedup_key
                    .iter()
                    .chain(message.dedup_aliases.iter())
                    .enumerate()
                {
                    transaction
                        .execute(
                            "INSERT INTO oc_aliases VALUES(?1,?2,?3,?4)",
                            params![source, ordinal as i64, position as i64 - 1, alias],
                        )
                        .unwrap();
                }
            }
        }
        transaction.commit().unwrap();
        let groups = sqlite
            .iter()
            .chain(json.iter())
            .map(opencode_identity_group);
        let mut oracle = OpenCodeSelection::new(opencode_authoritative_sources(groups));
        let mut expected: Vec<_> = sqlite
            .into_iter()
            .filter_map(|m| oracle.select_sqlite(m))
            .collect();
        expected.extend(json.into_iter().filter_map(|m| oracle.select_json(m, true)));
        expected.extend(oracle.finish());
        query_schema(&conn).unwrap();
        conn.execute("INSERT INTO ocq_scope VALUES(?1,0,1)", [&key])
            .unwrap();
        conn.execute("INSERT INTO ocq_scope VALUES(?1,0,0)", [&json_key])
            .unwrap();
        prepare_query(&conn, true).unwrap();
        let disk = DiskSelection { conn: &conn };
        let mut actual = Vec::new();
        let mut stmt=conn.prepare("SELECT id,kind,provider,message FROM ocq_rows ORDER BY kind DESC,source_order,ordinal").unwrap();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let id: i64 = row.get(0).unwrap();
            let kind: bool = row.get(1).unwrap();
            let provider: bool = row.get(2).unwrap();
            if if kind {
                disk.select_sqlite(id, provider).unwrap()
            } else {
                disk.select_json(id, provider, true).unwrap()
            } {
                let bytes: Vec<u8> = row.get(3).unwrap();
                actual.push(decode::<UnifiedMessage>(&bytes).unwrap());
            }
        }
        drop(rows);
        drop(stmt);
        let mut stmt=conn.prepare("SELECT message FROM ocq_rows r JOIN ocq_deferred d ON d.id=r.id WHERE d.active=1 ORDER BY r.id").unwrap();
        for data in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0)).unwrap() {
            actual.push(decode::<UnifiedMessage>(&data.unwrap()).unwrap());
        }
        for message in &mut expected {
            message.dedup_aliases.clear();
        }
        assert_eq!(encode(&expected).unwrap(), encode(&actual).unwrap());
        actual
    }

    #[test]
    fn disk_selection_matches_original_alias_and_reused_id_state_machine() {
        let base = opencode::parse_opencode_candidate(
            (
                "row".into(),
                "session".into(),
                usage(Some("embedded"), 3, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            true,
        )
        .unwrap()
        .message;
        let mut aliased = base.clone();
        aliased.dedup_aliases.push("legacy".into());
        let mut incompatible = aliased.clone();
        incompatible.tokens.input = 10;
        let mut authority = base.clone();
        authority.dedup_key = Some("legacy".into());
        authority.cost = 0.75;
        authority.mark_provider_reported_cost();
        let mut second_authority = base.clone();
        second_authority.cost = 0.5;
        second_authority.mark_provider_reported_cost();
        assert_disk_selection_matches_original(
            vec![base, aliased, incompatible, second_authority],
            vec![authority],
        );
    }

    #[test]
    fn late_alias_registration_preserves_original_deferred_source_queue() {
        let first = opencode::parse_opencode_candidate(
            (
                "row".into(),
                "session".into(),
                usage(Some("A"), 3, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            true,
        )
        .unwrap()
        .message;
        let mut second = first.clone();
        second.dedup_key = Some("B".into());
        second.tokens.input = 10;
        let mut late_alias = first.clone();
        late_alias.dedup_key = Some("B".into());
        late_alias.dedup_aliases.push("A".into());
        let mut authority = second.clone();
        authority.tokens.input = 25;
        authority.cost = 0.75;
        authority.mark_provider_reported_cost();
        let selected = assert_disk_selection_matches_original(
            vec![first, second, late_alias],
            vec![authority],
        );
        assert_eq!(
            selected
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            vec![25, 3]
        );
    }
    #[test]
    fn timestamp_separated_chain_reaches_authority_through_shared_aliases() {
        // Same key at a different timestamp is a different cohort: only links
        // sharing both fields extend the closure.
        let head = opencode::parse_opencode_candidate(
            (
                "head".into(),
                "session".into(),
                usage(Some("shared"), 3, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            true,
        )
        .unwrap()
        .message;
        let mut link = head.clone();
        link.dedup_key = Some("link".into());
        link.dedup_aliases.push("shared".into());
        link.timestamp = 1_700_000_000_001_000;
        let mut authority = link.clone();
        authority.dedup_key = Some("tail".into());
        authority.dedup_aliases = vec!["link".into()];
        authority.tokens.input = 25;
        authority.cost = 0.75;
        authority.mark_provider_reported_cost();
        let mut wrong_epoch = head.clone();
        wrong_epoch.dedup_aliases.push("link".into());
        wrong_epoch.timestamp = 1_700_000_001_000_000;
        let selected = assert_disk_selection_matches_original(
            vec![head, link, authority, wrong_epoch],
            vec![],
        );
        // Identity evidence (all SQLite): head [shared@T0]; link [link@T1,
        // shared@T1]; authority [tail@T1, link@T1] provider-reported;
        // wrong_epoch [shared@T2, link@T2]. Authority seed {tail@T1, link@T1}
        // closes over link to add shared@T1 only — shared@T0 and the T2 cohort
        // are different (key,timestamp) partitions. Selection: head emits (no
        // authority, marks shared@T0); link defers (authority, estimated);
        // provider authority emits with a new payload and does not consume the
        // deferred twin on the SQLite path; wrong_epoch emits (no authority,
        // no overlap). The deferred link twin stays queued and emits last via
        // finish, in registration order.
        assert_eq!(
            selected
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            vec![3, 25, 3, 3]
        );
    }
    #[test]
    fn alias_cycle_terminates_and_keeps_provider_fallback_order() {
        let first = opencode::parse_opencode_candidate(
            (
                "first".into(),
                "session".into(),
                usage(Some("A"), 3, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            true,
        )
        .unwrap()
        .message;
        let mut second = first.clone();
        second.dedup_key = Some("B".into());
        second.dedup_aliases.push("A".into());
        second.tokens.input = 10;
        let mut back = first.clone();
        back.dedup_key = Some("A".into());
        back.dedup_aliases.push("B".into());
        back.tokens.input = 11;
        let mut authority = second.clone();
        authority.tokens.input = 25;
        authority.cost = 0.75;
        authority.mark_provider_reported_cost();
        let selected =
            assert_disk_selection_matches_original(vec![first, second, back], vec![authority]);
        assert_eq!(
            selected
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            vec![25, 3, 11]
        );
    }
    #[test]
    fn provider_json_replaces_deferred_sqlite_twin_with_same_payload() {
        // Cost is outside payload identity: the estimated JSON copy stays
        // suppressed under authority while the provider-reported copy takes the
        // deferred SQLite twin through the exact-payload path.
        let sqlite = opencode::parse_opencode_candidate(
            (
                "row".into(),
                "session".into(),
                usage(Some("shared"), 3, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            true,
        )
        .unwrap()
        .message;
        let estimated_json = sqlite.clone();
        let mut reported_json = sqlite.clone();
        reported_json.cost = 0.75;
        reported_json.mark_provider_reported_cost();
        let oracle_selected = assert_disk_selection_matches_original(
            vec![sqlite],
            vec![estimated_json, reported_json],
        );
        assert_eq!(oracle_selected.len(), 1);
        assert_eq!(oracle_selected[0].tokens.input, 3);
        assert_eq!(oracle_selected[0].cost, 0.75);
        assert_eq!(oracle_selected[0].cost_source, CostSource::ProviderReported);
    }

    #[test]
    fn float_bits_ids_and_large_alias_cohort_stay_on_disk() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        source.execute_batch("BEGIN").unwrap();
        for index in 0..2049 {
            insert(
                &source,
                &format!("a-fork-{index:04}"),
                usage(None, 3, 0.0, 1_700_000_000_000.25),
            );
        }
        insert(
            &source,
            "unique-a",
            usage(Some("a"), 3, 0.0, 1_700_000_000_010.25),
        );
        insert(
            &source,
            "unique-b",
            usage(Some("b"), 3, 0.0, 1_700_000_000_010.25),
        );
        insert(
            &source,
            "float-c",
            usage(Some("c"), 3, 0.0, 1_700_000_000_020.25),
        );
        insert(
            &source,
            "float-c-copy",
            usage(Some("c"), 3, 0.0, 1_700_000_000_020.75),
        );
        source.execute_batch("COMMIT").unwrap();
        let cache = root.path().join("cache");
        let paths = [path.clone()];
        let output = collect(
            &paths,
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        // The last two original-float cohorts have the same integer payload and
        // same source key, so cross-source selection collapses them only later.
        assert_eq!(output.len(), 4);
        assert!(output
            .iter()
            .all(|message| message.dedup_aliases.is_empty()));
        let conn = open_database(&cache).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM oc_messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5);
        let aliases: i64 = conn
            .query_row("SELECT count(*) FROM oc_aliases WHERE ordinal=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(aliases, 2049);
        let raw: Vec<u8> = conn
            .query_row("SELECT message FROM oc_messages WHERE ordinal=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(raw.len() < 4096);
        let explicit = materialize_opencode_source(&path).unwrap();
        assert_eq!(explicit.len(), 5);
        assert_eq!(explicit[0].dedup_aliases.len(), 2048);
        for mode in [
            OpenCodeSelectionMode::Materialized,
            OpenCodeSelectionMode::Report,
        ] {
            let mut hydrated = Vec::new();
            visit_opencode_messages(&paths, &[], &cache, mode, true, &|_| true, &mut |message| {
                hydrated.push(message.clone())
            })
            .unwrap();
            assert_eq!(hydrated.len(), output.len());
            assert_eq!(hydrated[0].dedup_aliases.len(), 2048);
            assert_eq!(hydrated[0].dedup_aliases, explicit[0].dedup_aliases);
            for message in &mut hydrated {
                message.dedup_aliases.clear();
            }
            assert_eq!(encode(&hydrated).unwrap(), encode(&output).unwrap());
        }
    }

    #[test]
    fn retained_backup_is_not_a_second_source_and_recovery_never_overwrites() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let conn = open_database(&cache).unwrap();
        conn.execute_batch("CREATE TABLE retained_fixture(key TEXT PRIMARY KEY,payload BLOB); INSERT INTO retained_fixture VALUES('only-copy',x'010203');").unwrap();
        refresh_retained_recovery(&conn, &cache).unwrap();
        assert!(recover_database(&cache).is_err());
        let source =
            Connection::open_with_flags(cache.join(RECOVERY), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let value: Vec<u8> = source
            .query_row("SELECT payload FROM retained_fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, vec![1, 2, 3]);
        drop(source);
        drop(conn);
        fs::rename(
            cache.join(DATABASE),
            cache.join("preserved-invalid.sqlite3"),
        )
        .unwrap();
        recover_database(&cache).unwrap();
        let restored = open_database(&cache).unwrap();
        let value: Vec<u8> = restored
            .query_row("SELECT payload FROM retained_fixture", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, vec![1, 2, 3]);
    }

    #[cfg(unix)]
    #[test]
    fn warm_admission_preserves_cross_process_writer_lock() {
        const CHILD_ROOT: &str = "TOKSCALE_TEST_ADMISSION_LOCK_ROOT";
        const CHILD_BUSY: &str = "TOKSCALE_TEST_ADMISSION_EXPECT_BUSY";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let child = Connection::open_with_flags(
                sqlite_open_path(&PathBuf::from(root).join(DATABASE)).unwrap(),
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            child.busy_timeout(Duration::from_millis(50)).unwrap();
            let result = child.execute_batch("BEGIN IMMEDIATE");
            if std::env::var_os(CHILD_BUSY).is_some() {
                assert!(
                    matches!(result, Err(rusqlite::Error::SqliteFailure(code, _))
                    if code.code == rusqlite::ErrorCode::DatabaseBusy),
                    "warm admission released another connection's process locks"
                );
            } else {
                result.unwrap();
                child.execute_batch("ROLLBACK").unwrap();
            }
            return;
        }
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let writer = open_database(&cache).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE lock_fixture(value INTEGER);
            BEGIN IMMEDIATE; INSERT INTO lock_fixture VALUES(1);",
            )
            .unwrap();
        let _warm = open_database(&cache).unwrap();
        let probe = |busy: bool| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.env_clear().env(CHILD_ROOT, &cache).args([
                "--exact",
                "usage_store::tests::warm_admission_preserves_cross_process_writer_lock",
                "--nocapture",
            ]);
            if busy {
                command.env(CHILD_BUSY, "1");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "isolated lock probe failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        probe(true);
        writer.execute_batch("COMMIT").unwrap();
        probe(false);
    }

    #[cfg(unix)]
    #[test]
    fn opened_database_identity_rejects_replaced_and_hardlinked_paths() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let connection = open_database(&cache).unwrap();
        let path = cache.join(DATABASE);
        let original = database_path_metadata(&path).unwrap().unwrap();
        let alias = cache.join("alias.db");
        fs::hard_link(&path, &alias).unwrap();
        assert!(database_path_metadata(&path).is_err());
        fs::remove_file(&alias).unwrap();
        fs::rename(&path, cache.join("preserved.db")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(admit_open_database(&connection, &path, Some(&original)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_database_rejects_symlinks_and_is_private() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        secure_directory(&cache).unwrap();
        let target = root.path().join("unrelated");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, cache.join(DATABASE)).unwrap();
        assert!(open_database(&cache).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched");
        fs::remove_file(cache.join(DATABASE)).unwrap();
        drop(open_database(&cache).unwrap());
        assert_eq!(
            fs::metadata(cache.join(DATABASE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn nofollow_sqlite_open_accepts_ancestor_alias_but_rejects_cache_root_alias() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        let ancestor_alias = root.path().join("ancestor-alias");
        symlink(&real, &ancestor_alias).unwrap();
        let native_cache = ancestor_alias.join("cache");
        let connection = open_database(&native_cache).unwrap();
        let opened_path = fs::canonicalize(real.join("cache")).unwrap().join(DATABASE);
        assert_eq!(connection.path(), opened_path.to_str());
        assert_eq!(
            native_cache,
            ancestor_alias.join("cache"),
            "native scope path stays unchanged"
        );
        let root_alias = root.path().join("cache-root-alias");
        symlink(&native_cache, &root_alias).unwrap();
        assert!(open_database(&root_alias).is_err());
    }

    #[test]
    fn pinned_scope_survives_later_source_failure_and_never_reopens_sources() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let a = root.path().join("a.db");
        let b = root.path().join("b.db");
        let source_a = fixture(&a, false);
        let source_b = fixture(&b, false);
        insert(
            &source_a,
            "a",
            usage(Some("a"), 3, 0.0, 1_700_000_000_000.0),
        );
        insert(
            &source_b,
            "b",
            usage(Some("b"), 4, 0.0, 1_700_000_000_001.0),
        );
        let mut store = open_database(&cache).unwrap();
        store.execute_batch("CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,complete INTEGER NOT NULL);
            CREATE TABLE sq_scope_sources(scope_generation INTEGER NOT NULL,kind TEXT NOT NULL,source_order INTEGER NOT NULL,path BLOB NOT NULL,generation BLOB NOT NULL,PRIMARY KEY(scope_generation,kind,source_order));
            INSERT INTO sq_scope_generations VALUES(1,0);INSERT INTO sq_scope_generations VALUES(2,0);").unwrap();
        let register = |scope: i64,
                        conn: &Connection,
                        path: &Path,
                        sqlite: bool,
                        order: usize,
                        generation: &[u8]| {
            sql(conn.execute(
                "INSERT INTO sq_scope_sources VALUES(?1,?2,?3,?4,?5)",
                params![
                    scope,
                    if sqlite {
                        "opencode-sqlite"
                    } else {
                        "opencode-json"
                    },
                    order as i64,
                    source_key(path)?,
                    generation
                ],
            ))?;
            Ok(())
        };
        let paths = [a.clone(), b.clone()];
        synchronize_opencode_sources(
            &paths,
            &[],
            &cache,
            &mut |conn, path, kind, order, generation| {
                register(1, conn, path, kind, order, generation)
            },
        )
        .unwrap();
        store
            .execute("UPDATE sq_scope_generations SET complete=1 WHERE id=1", [])
            .unwrap();
        let mut expected = Vec::new();
        visit_opencode_snapshot(
            &cache,
            1,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |message| expected.push(message.clone()),
        )
        .unwrap();
        assert_eq!(
            expected
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        let mut pinned_connection = open_database(&cache).unwrap();
        let mut unpinned_emitted = 0;
        assert!(visit_opencode_snapshot_on(
            &pinned_connection,
            1,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |_| unpinned_emitted += 1
        )
        .is_err());
        assert_eq!(unpinned_emitted, 0);
        let pinned = pinned_connection.transaction().unwrap();
        let mut first_pass = Vec::new();
        visit_opencode_snapshot_on(
            &pinned,
            1,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |message| first_pass.push(message.clone()),
        )
        .unwrap();
        assert_eq!(encode(&first_pass).unwrap(), encode(&expected).unwrap());
        insert(
            &source_a,
            "a",
            usage(Some("a"), 30, 0.0, 1_700_000_000_000.0),
        );
        source_b
            .execute("UPDATE message SET data='invalid json'", [])
            .unwrap();
        assert!(synchronize_opencode_sources(
            &paths,
            &[],
            &cache,
            &mut |conn, path, kind, order, generation| register(
                2, conn, path, kind, order, generation
            )
        )
        .is_err());
        let new_a:Vec<u8>=store.query_row("SELECT generation FROM sq_scope_sources WHERE scope_generation=2 AND source_order=0",[],|row|row.get(0)).unwrap();
        let mut incomplete_emitted = 0;
        assert!(visit_opencode_snapshot(
            &cache,
            2,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |_| incomplete_emitted += 1
        )
        .is_err());
        assert_eq!(incomplete_emitted, 0);
        // Failure recovery and pure queries must not reopen the original DBs.
        drop(source_a);
        drop(source_b);
        fs::rename(&a, root.path().join("a-offline.db")).unwrap();
        fs::rename(&b, root.path().join("b-offline.db")).unwrap();
        let mut actual = Vec::new();
        visit_opencode_snapshot(
            &cache,
            1,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |message| actual.push(message.clone()),
        )
        .unwrap();
        assert_eq!(encode(&actual).unwrap(), encode(&expected).unwrap());
        let transaction = store.transaction().unwrap();
        discard_unreferenced_opencode_generations(&transaction).unwrap();
        transaction.commit().unwrap();
        actual.clear();
        visit_opencode_snapshot(
            &cache,
            1,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |message| actual.push(message.clone()),
        )
        .unwrap();
        assert_eq!(actual, expected);
        let old_b: Vec<u8> = store
            .query_row(
                "SELECT generation FROM sq_scope_sources
             WHERE scope_generation=1 AND source_order=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Replacing query scopes must not remove the latest collected facts.
        let transaction = store.transaction().unwrap();
        transaction
            .execute("DELETE FROM sq_scope_sources WHERE scope_generation=1", [])
            .unwrap();
        transaction
            .execute("DELETE FROM sq_scope_generations WHERE id=1", [])
            .unwrap();
        discard_unreferenced_opencode_generations(&transaction).unwrap();
        transaction.commit().unwrap();
        store
            .execute("INSERT INTO sq_scope_generations VALUES(3,1)", [])
            .unwrap();
        register(3, &store, &a, true, 0, &new_a).unwrap();
        register(3, &store, &b, true, 1, &old_b).unwrap();
        let mut recovered = Vec::new();
        visit_opencode_snapshot(
            &cache,
            3,
            OpenCodeSelectionMode::Materialized,
            false,
            &|_| true,
            &mut |message| recovered.push(message.clone()),
        )
        .unwrap();
        assert_eq!(
            recovered
                .iter()
                .map(|message| message.tokens.input)
                .collect::<Vec<_>>(),
            vec![30, 4],
        );
        // Another connection has now deleted the old scope and source rows.
        // Native SQLite snapshot pinning preserves exact inference/totals
        // parity without durable reader leases or reopening original sources.
        for _ in 0..2 {
            let mut later_pass = Vec::new();
            visit_opencode_snapshot_on(
                &pinned,
                1,
                OpenCodeSelectionMode::Materialized,
                false,
                &|_| true,
                &mut |message| later_pass.push(message.clone()),
            )
            .unwrap();
            assert_eq!(encode(&later_pass).unwrap(), encode(&expected).unwrap());
        }
    }

    #[test]
    fn sqlite_full_during_source_staging_preserves_last_good_facts() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        insert(&source, "a", usage(Some("a"), 3, 0.0, 1_700_000_000_000.0));
        let mut store = open_database(&cache).unwrap();
        schema(&store).unwrap();
        let old_generation = sync_source(&mut store, &path, true, &mut |_, _| Ok(())).unwrap();
        let fingerprint = encoded_source(&store, &path);
        let pages: i64 = store
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        store.pragma_update(None, "max_page_count", pages).unwrap();
        source.execute_batch("BEGIN").unwrap();
        for index in 0..300 {
            insert(
                &source,
                &format!("new-{index:03}"),
                usage(
                    Some(&format!("new-{index}")),
                    4,
                    0.0,
                    1_700_000_001_000.0 + index as f64,
                ),
            );
        }
        source.execute_batch("COMMIT").unwrap();
        assert!(sync_source(&mut store, &path, true, &mut |_, _| Ok(())).is_err());
        assert_eq!(fingerprint, encoded_source(&store, &path));
        let count: i64 = store
            .query_row("SELECT count(*) FROM oc_generations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let blob: Vec<u8> = store
            .query_row(
                "SELECT message FROM oc_messages WHERE source=?1 AND ordinal=0",
                [old_generation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(decode::<UnifiedMessage>(&blob).unwrap().tokens.input, 3);
    }

    #[test]
    fn failed_recovery_backup_preserves_previous_unique_history() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let store = open_database(&cache).unwrap();
        store.execute_batch("CREATE TABLE retained_fixture(payload BLOB);INSERT INTO retained_fixture VALUES(x'010203');").unwrap();
        refresh_retained_recovery(&store, &cache).unwrap();
        store
            .execute("UPDATE retained_fixture SET payload=x'040506'", [])
            .unwrap();
        let recovery = Connection::open(cache.join(RECOVERY)).unwrap();
        let mode: String = recovery
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(
            mode, "delete",
            "backup must not inherit the canonical WAL header"
        );
        recovery.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(refresh_retained_recovery(&store, &cache).is_err());
        let old: Vec<u8> = recovery
            .query_row("SELECT payload FROM retained_fixture", [], |row| row.get(0))
            .unwrap();
        assert_eq!(old, vec![1, 2, 3]);
        recovery.execute_batch("ROLLBACK").unwrap();
        refresh_retained_recovery(&store, &cache).unwrap();
        let new: Vec<u8> = recovery
            .query_row("SELECT payload FROM retained_fixture", [], |row| row.get(0))
            .unwrap();
        assert_eq!(new, vec![4, 5, 6]);
    }

    #[test]
    fn prepared_query_owns_aliases_after_canonical_gc_and_allows_callback_writes() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        insert(
            &source,
            "",
            usage(Some("embedded"), 3, 0.0, 1_700_000_000_000.0),
        );
        insert(
            &source,
            "alias",
            usage(Some("embedded"), 3, 0.0, 1_700_000_000_000.0),
        );
        let expected = opencode::parse_opencode_sqlite(&path);
        let mut reader = open_database(&cache).unwrap();
        schema(&reader).unwrap();
        query_schema(&reader).unwrap();
        sync_source(&mut reader, &path, true, &mut |conn, generation| {
            sql(conn.execute("INSERT INTO ocq_scope VALUES(?1,0,1)", [generation]))?;
            Ok(())
        })
        .unwrap();
        let transaction = reader.transaction().unwrap();
        prepare_query(&transaction, true).unwrap();
        transaction.commit().unwrap();
        let writer = open_database(&cache).unwrap();
        writer.execute_batch("DELETE FROM oc_aliases; DELETE FROM oc_messages; DELETE FROM oc_rows; DELETE FROM oc_sources; DELETE FROM oc_generations;").unwrap();
        for _ in 0..2 {
            let mut observed = Vec::new();
            visit_prepared_opencode_snapshot_on(
                &reader,
                OpenCodeSelectionMode::Materialized,
                true,
                &|_| {},
                &|_| {
                    writer
                        .execute_batch(
                            "BEGIN IMMEDIATE; UPDATE oc_meta SET version=version; COMMIT;",
                        )
                        .unwrap();
                    true
                },
                &mut |message| observed.push(message.clone()),
            )
            .unwrap();
            assert_eq!(observed, expected);
        }
        let mut prepared_output = Vec::new();
        visit_opencode_query_on(
            &reader,
            false,
            OpenCodeSelectionMode::Report,
            true,
            &|message| {
                message.cost += 1.0;
                message.date = "prepared-date".into();
            },
            &|message| message.cost == 1.0 && message.date == "prepared-date",
            &mut |message| prepared_output.push(message.clone()),
        )
        .unwrap();
        let mut prepared_expected = expected;
        for message in &mut prepared_expected {
            message.cost += 1.0;
            message.date = "prepared-date".into();
        }
        assert_eq!(prepared_output, prepared_expected);
    }

    #[test]
    fn explicit_materialization_is_independent_of_real_scratch_sqlite_full() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        source.execute_batch("BEGIN").unwrap();
        for index in 0..300 {
            insert(
                &source,
                &format!("row-{index:03}"),
                usage(
                    Some(&format!("embedded-{index}")),
                    index + 1,
                    0.25,
                    1_700_000_000_000.0 + index as f64,
                ),
            );
        }
        source.execute_batch("COMMIT").unwrap();
        let mut scratch = Connection::open("").unwrap();
        configure_connection(&scratch).unwrap();
        schema(&scratch).unwrap();
        let pages: i64 = scratch
            .pragma_query_value(None, "page_count", |row| row.get(0))
            .unwrap();
        scratch
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let error = sync_source(&mut scratch, &path, true, &mut |_, _| Ok(())).unwrap_err();
        assert!(error.contains("database operation failed"));
        let memory = materialization_database().unwrap();
        let journal: String = memory
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let temp: i64 = memory
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "memory");
        assert_eq!(temp, 2);
        let messages = opencode::parse_opencode_sqlite(&path);
        assert_eq!(messages.len(), 300);
        assert_eq!(
            messages
                .iter()
                .map(|message| message.tokens.input)
                .sum::<i64>(),
            (1..=300).sum::<i64>()
        );
        assert!(messages
            .iter()
            .all(|message| message.cost == 0.25
                && message.cost_source == CostSource::ProviderReported));
        assert_eq!(messages[0].dedup_key.as_deref(), Some("embedded-0"));
        assert_eq!(messages[299].dedup_key.as_deref(), Some("embedded-299"));
    }

    #[test]
    fn opaque_alias_predicates_observe_candidates_and_report_fallbacks() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let path = root.path().join("source.db");
        let source = fixture(&path, false);
        insert(
            &source,
            "legacy-0",
            usage(None, 3, 0.0, 1_700_000_000_000.0),
        );
        insert(
            &source,
            "shared-json",
            usage(None, 3, 0.0, 1_700_000_000_000.0),
        );
        let json = root.path().join("shared-json.json");
        fs::write(
            &json,
            usage(Some("shared-json"), 3, 0.75, 1_700_000_000_000.0).to_string(),
        )
        .unwrap();
        let paths = [path];
        let files = [json];
        let alias_only = |message: &UnifiedMessage| {
            message
                .dedup_aliases
                .iter()
                .any(|alias| alias == "shared-json")
        };
        let mut report = Vec::new();
        visit_opencode_messages(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Report,
            true,
            &alias_only,
            &mut |message| report.push(message.clone()),
        )
        .unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].cost, 0.0);
        assert_eq!(report[0].dedup_aliases, vec!["shared-json"]);
        let mut materialized = Vec::new();
        visit_opencode_messages(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Materialized,
            true,
            &alias_only,
            &mut |message| materialized.push(message.clone()),
        )
        .unwrap();
        assert!(materialized.is_empty());
        let mut compact = Vec::new();
        visit_opencode_messages(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Report,
            false,
            &alias_only,
            &mut |message| compact.push(message.clone()),
        )
        .unwrap();
        assert!(compact.is_empty());
        // SQLite authority still marks identities before the output filter.
        // An alias-empty predicate must not make a filtered SQLite winner allow
        // its JSON twin to emit or see an incorrectly compacted candidate.
        insert(
            &source,
            "legacy-0",
            usage(None, 3, 0.5, 1_700_000_000_000.0),
        );
        let mut selected = Vec::new();
        visit_opencode_messages(
            &paths,
            &files,
            &cache,
            OpenCodeSelectionMode::Report,
            true,
            &|message| message.dedup_aliases.is_empty(),
            &mut |message| selected.push(message.clone()),
        )
        .unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn scope_eviction_preserves_live_sources_and_absence_reconciliation_is_scoped() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let source_dir = root.path().join("source-a");
        fs::create_dir(&source_dir).unwrap();
        let a = source_dir.join("source.db");
        let b = root.path().join("unrelated.db");
        let source_a = fixture(&a, false);
        let source_b = fixture(&b, false);
        insert(
            &source_a,
            "a",
            usage(Some("a"), 3, 0.0, 1_700_000_000_000.0),
        );
        insert(
            &source_b,
            "b",
            usage(Some("b"), 4, 0.0, 1_700_000_000_001.0),
        );
        let mut store = open_database(&cache).unwrap();
        store.execute_batch("CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,scope_id INTEGER NOT NULL,complete INTEGER NOT NULL);
            CREATE TABLE sq_scope_sources(scope_generation INTEGER NOT NULL,kind TEXT NOT NULL,source_order INTEGER NOT NULL,path BLOB NOT NULL,generation BLOB NOT NULL,PRIMARY KEY(scope_generation,kind,source_order));
            INSERT INTO sq_scope_generations VALUES(1,10,0),(2,10,0),(3,10,0),(4,10,0),(5,20,0);").unwrap();
        let register = |scope: i64,
                        conn: &Connection,
                        path: &Path,
                        sqlite: bool,
                        order: usize,
                        generation: &[u8]| {
            sql(conn.execute(
                "INSERT INTO sq_scope_sources VALUES(?1,?2,?3,?4,?5)",
                params![
                    scope,
                    if sqlite {
                        "opencode-sqlite"
                    } else {
                        "opencode-json"
                    },
                    order as i64,
                    source_key(path)?,
                    generation
                ],
            ))?;
            Ok(())
        };
        synchronize_opencode_sources(
            std::slice::from_ref(&a),
            &[],
            &cache,
            &mut |conn, path, kind, order, generation| {
                register(1, conn, path, kind, order, generation)
            },
        )
        .unwrap();
        synchronize_opencode_sources(
            std::slice::from_ref(&b),
            &[],
            &cache,
            &mut |conn, path, kind, order, generation| {
                register(5, conn, path, kind, order, generation)
            },
        )
        .unwrap();
        store
            .execute(
                "UPDATE sq_scope_generations SET complete=1 WHERE id IN (1,5)",
                [],
            )
            .unwrap();
        let original_generation: Vec<u8> = store
            .query_row(
                "SELECT generation FROM oc_sources WHERE path=?1",
                [source_key(&a).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let transaction = store.transaction().unwrap();
        reconcile_missing_opencode_sources_on(&transaction, 1, 2).unwrap();
        transaction
            .execute("DELETE FROM sq_scope_sources WHERE scope_generation=1", [])
            .unwrap();
        transaction
            .execute("DELETE FROM sq_scope_generations WHERE id=1", [])
            .unwrap();
        discard_unreferenced_opencode_generations(&transaction).unwrap();
        transaction.commit().unwrap();
        let evicted_generation: Vec<u8> = store
            .query_row(
                "SELECT generation FROM oc_sources WHERE path=?1",
                [source_key(&a).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(original_generation, evicted_generation);
        synchronize_opencode_sources(
            std::slice::from_ref(&a),
            &[],
            &cache,
            &mut |conn, path, kind, order, generation| {
                register(3, conn, path, kind, order, generation)
            },
        )
        .unwrap();
        let warm_generation: Vec<u8> = store
            .query_row(
                "SELECT generation FROM sq_scope_sources WHERE scope_generation=3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(original_generation, warm_generation);
        store
            .execute("UPDATE sq_scope_generations SET complete=1 WHERE id=3", [])
            .unwrap();
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::metadata(&source_dir).unwrap().permissions();
            fs::set_permissions(&source_dir, fs::Permissions::from_mode(0o000)).unwrap();
            let transaction = store.transaction().unwrap();
            let result = reconcile_missing_opencode_sources_on(&transaction, 3, 4);
            drop(transaction);
            fs::set_permissions(&source_dir, permissions).unwrap();
            assert!(result.is_err());
            let current: Vec<u8> = store
                .query_row(
                    "SELECT generation FROM oc_sources WHERE path=?1",
                    [source_key(&a).unwrap()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(original_generation, current);
        }
        drop(source_a);
        drop(source_b);
        fs::rename(&a, source_dir.join("preserved.db")).unwrap();
        fs::rename(&b, root.path().join("preserved-unrelated.db")).unwrap();
        let transaction = store.transaction().unwrap();
        assert!(reconcile_missing_opencode_sources_on(&transaction, 3, 5).is_err());
        drop(transaction);
        let transaction = store.transaction().unwrap();
        reconcile_missing_opencode_sources_on(&transaction, 3, 4).unwrap();
        transaction
            .execute("UPDATE sq_scope_generations SET complete=1 WHERE id=4", [])
            .unwrap();
        transaction
            .execute("DELETE FROM sq_scope_sources WHERE scope_generation=3", [])
            .unwrap();
        transaction
            .execute("DELETE FROM sq_scope_generations WHERE id=3", [])
            .unwrap();
        discard_unreferenced_opencode_generations(&transaction).unwrap();
        transaction.commit().unwrap();
        let archived = collect(
            &[a, b],
            &[],
            &cache,
            OpenCodeSelectionMode::Materialized,
            &|_| true,
        )
        .unwrap();
        let mut inputs: Vec<_> = archived
            .iter()
            .map(|message| message.tokens.input)
            .collect();
        inputs.sort_unstable();
        assert_eq!(inputs, vec![3, 4]);
    }

    #[test]
    fn pinned_scope_survives_late_writes_and_rejects_missing_generations() {
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let source_dir = root.path().join("sources");
        fs::create_dir(&source_dir).unwrap();
        let db = source_dir.join("source.db");
        let json = source_dir.join("j.json");
        let source = fixture(&db, true);
        insert(
            &source,
            "row",
            usage(Some("row"), 3, 0.0, 1_700_000_000_000.0),
        );
        let original_json = usage(Some("j"), 4, 0.25, 1_700_000_000_001.0).to_string();
        fs::write(&json, &original_json).unwrap();
        let store = open_database(&cache).unwrap();
        store.execute_batch("CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,complete INTEGER NOT NULL);
            CREATE TABLE sq_scope_sources(scope_generation INTEGER NOT NULL,kind TEXT NOT NULL,source_order INTEGER NOT NULL,path BLOB NOT NULL,generation BLOB NOT NULL,PRIMARY KEY(scope_generation,kind,source_order));
            INSERT INTO sq_scope_generations VALUES(1,0),(2,0),(3,0);").unwrap();
        let stage = |scope: i64| {
            synchronize_opencode_sources(
                std::slice::from_ref(&db),
                std::slice::from_ref(&json),
                &cache,
                &mut |conn, path, sqlite, order, generation| {
                    sql(conn.execute(
                        "INSERT INTO sq_scope_sources VALUES(?1,?2,?3,?4,?5)",
                        params![
                            scope,
                            if sqlite {
                                "opencode-sqlite"
                            } else {
                                "opencode-json"
                            },
                            order as i64,
                            source_key(path)?,
                            generation
                        ],
                    ))?;
                    Ok(())
                },
            )
        };
        let inputs = |scope| {
            let mut statement = store.prepare(
                "SELECT m.message FROM sq_scope_sources q JOIN oc_messages m ON m.source=q.generation WHERE q.scope_generation=?1",
            ).unwrap();
            let rows = statement
                .query_map([scope], |row| row.get::<_, Vec<u8>>(0))
                .unwrap();
            rows.map(|row| {
                decode::<UnifiedMessage>(&row.unwrap())
                    .unwrap()
                    .tokens
                    .input
            })
            .sum::<i64>()
        };
        stage(1).unwrap();
        validate_opencode_scope_sources_on(&store, 1).unwrap();
        assert_eq!(inputs(1), 7);

        // An independent writer changes the source after its consistent read.
        // The captured facts remain valid; the next scope observes the WAL.
        insert(
            &source,
            "row",
            usage(Some("row"), 31, 0.0, 1_700_000_000_000.0),
        );
        validate_opencode_scope_sources_on(&store, 1).unwrap();
        assert_eq!(inputs(1), 7);
        stage(2).unwrap();
        validate_opencode_scope_sources_on(&store, 2).unwrap();
        assert_eq!(inputs(2), 35);
        assert_eq!(inputs(1), 7);

        fs::remove_file(&json).unwrap();
        validate_opencode_scope_sources_on(&store, 2).unwrap();
        assert_eq!(inputs(2), 35);
        // Durable staging carries the absent JSON source's canonical rows.
        stage(3).unwrap();
        validate_opencode_scope_sources_on(&store, 3).unwrap();
        assert_eq!(inputs(2), 35);
        assert!(validate_opencode_scope_sources_on(&store, 99).is_err());

        store.execute(
            "UPDATE sq_scope_sources SET generation=x'00' WHERE scope_generation=2 AND kind='opencode-sqlite'",
            [],
        ).unwrap();
        assert!(validate_opencode_scope_sources_on(&store, 2).is_err());
        validate_opencode_scope_sources_on(&store, 1).unwrap();
        assert_eq!(inputs(1), 7);
    }

    #[test]
    fn carried_absent_db_orders_follow_live_without_collision() {
        // Mixed live+deleted DBs preserve all prior contributions: the
        // deleted DB's canonical generation re-registers after the live
        // orders, never at order 0 (SQLite PK collision).
        let root = TempDir::new().unwrap();
        let cache = root.path().join("cache");
        let live_path = root.path().join("opencode-live.db");
        let gone_path = root.path().join("opencode-gone.db");
        let live_source = fixture(&live_path, false);
        let gone_source = fixture(&gone_path, false);
        insert(
            &live_source,
            "live",
            usage(Some("live"), 10, 0.0, 1_700_000_000_000.0),
        );
        insert(
            &gone_source,
            "gone",
            usage(Some("gone"), 20, 0.0, 1_700_000_000_001.0),
        );
        drop(live_source);
        drop(gone_source);
        let store = open_database(&cache).unwrap();
        // Staged-scope shape the real register_opencode writes: one lane,
        // per-kind source_order.
        store.execute_batch("CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,complete INTEGER NOT NULL);
            CREATE TABLE sq_scope_sources(scope_generation INTEGER NOT NULL,kind TEXT NOT NULL,lane TEXT NOT NULL DEFAULT 'opencode',source_order INTEGER NOT NULL,path BLOB NOT NULL,generation BLOB NOT NULL,PRIMARY KEY(scope_generation,kind,lane,source_order));
            INSERT INTO sq_scope_generations VALUES(1,0);").unwrap();
        let scope = std::cell::Cell::new(1);
        let mut register =
            |conn: &Connection, path: &Path, sqlite: bool, order: usize, generation: &[u8]| {
                sql(conn.execute(
                    "INSERT INTO sq_scope_sources VALUES(?1,?2,'opencode',?3,?4,?5)",
                    params![
                        scope.get(),
                        if sqlite {
                            "opencode-sqlite"
                        } else {
                            "opencode-json"
                        },
                        order as i64,
                        source_key(path)?,
                        generation
                    ],
                ))?;
                Ok(())
            };
        synchronize_opencode_sources(
            &[live_path.clone(), gone_path.clone()],
            &[],
            &cache,
            &mut register,
        )
        .unwrap();
        // Second scope: gone.db deleted; live.db stays.
        std::fs::remove_file(&gone_path).unwrap();
        store
            .execute("INSERT INTO sq_scope_generations VALUES(2,0)", [])
            .unwrap();
        scope.set(2);
        synchronize_opencode_sources(std::slice::from_ref(&live_path), &[], &cache, &mut register)
            .unwrap();
        let scan = crate::scanner::ScanResult {
            durable_opencode_data_dir: Some(root.path().to_path_buf()),
            ..Default::default()
        };
        carry_absent_opencode_sources(&scan, &[live_path], &[], &cache, 2, &mut register).unwrap();
        let orders: Vec<i64> = store
            .prepare("SELECT source_order FROM sq_scope_sources WHERE scope_generation=2 AND kind='opencode-sqlite' ORDER BY source_order")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(orders, vec![0, 1]);
        // Both contributions survive: live 10 + carried 20.
        let total: i64 = store
            .prepare("SELECT m.message FROM sq_scope_sources q JOIN oc_messages m ON m.source=q.generation WHERE q.scope_generation=2")
            .unwrap()
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|row| decode::<UnifiedMessage>(&row.unwrap()).unwrap().tokens.input)
            .sum();
        assert_eq!(total, 30);
    }
    #[test]
    fn carried_missing_update_delete_empty_keep_alias_and_multiplicity_across_batch_boundary() {
        // Fresh update/delete keep exact retained/dedup totals; the carry
        // streams the prior generation in bounded ordinal pages, so a changed
        // source missing >1024 rows still carries everything without any
        // whole-history Rust collection.
        let scratch = Connection::open("").unwrap();
        configure_connection(&scratch).unwrap();
        schema(&scratch).unwrap();
        let fresh: Vec<u8> = vec![7; 32];
        let previous: Vec<u8> = vec![9; 32];
        let seed = opencode::parse_opencode_candidate(
            (
                "seed".into(),
                "session".into(),
                usage(Some("seed"), 1, 0.0, 1_700_000_000_000.0).to_string(),
                None,
            ),
            false,
        )
        .unwrap();
        let candidate = |dedup: &str, aliases: &[&str], input: i64| OpenCodeCandidate {
            message: {
                let mut message = opencode::parse_opencode_candidate(
                    (
                        dedup.into(),
                        "session".into(),
                        usage(Some(dedup), input, 0.0, 1_700_000_000_000.0).to_string(),
                        None,
                    ),
                    false,
                )
                .unwrap()
                .message;
                message.dedup_aliases = aliases.iter().map(|alias| alias.to_string()).collect();
                message
            },
            fingerprint: seed.fingerprint.clone(),
            embedded_id: Some(dedup.into()),
            is_v1: false,
        };
        let prior_total: i64 = 2200;
        for ordinal in 0..prior_total {
            // Even ordinals share one alias; ordinal 6 duplicates the key of
            // ordinal 5 (without the alias) to pin identical carried
            // multiplicity.
            let (dedup, aliases) = if ordinal == 6 {
                ("row-0005".to_string(), vec![])
            } else if ordinal % 2 == 0 {
                (format!("row-{ordinal:04}"), vec!["shared-alias"])
            } else {
                (format!("row-{ordinal:04}"), vec![])
            };
            scratch
                .execute(
                    "INSERT INTO oc_rows VALUES(?1,?2,?3)",
                    params![
                        previous,
                        ordinal,
                        encode(&candidate(&dedup, &aliases, ordinal + 1)).unwrap()
                    ],
                )
                .unwrap();
        }
        // Fresh generation: same-key updates (row-0001, row-0002), an
        // alias-only join (fresh-alias-holder via shared-alias), one new row.
        for (ordinal, dedup, aliases, input) in [
            (0, "row-0001", &[][..], 10_001),
            (1, "row-0002", &[][..], 10_002),
            (2, "fresh-alias-holder", &["shared-alias"][..], 10_003),
            (3, "fresh-new", &[][..], 10_004),
        ] {
            scratch
                .execute(
                    "INSERT INTO oc_rows VALUES(?1,?2,?3)",
                    params![
                        fresh,
                        ordinal,
                        encode(&candidate(dedup, aliases, input)).unwrap()
                    ],
                )
                .unwrap();
        }
        carry_missing_opencode_candidates(&scratch, &fresh, &previous).unwrap();
        let rows: Vec<(i64, OpenCodeCandidate)> = {
            let mut statement = scratch
                .prepare("SELECT ordinal,candidate FROM oc_rows WHERE source=?1 ORDER BY ordinal")
                .unwrap();
            let mut rows = statement.query([&fresh]).unwrap();
            let mut out = Vec::new();
            while let Some(row) = rows.next().unwrap() {
                out.push((row.get(0).unwrap(), decode_row(row, 1).unwrap()));
            }
            out
        };
        // Odd prior ordinals except 1 carry, plus the even ordinal-6
        // duplicate: 1100 carried rows after the 4 fresh ones.
        assert_eq!(rows.len(), 1104);
        assert_eq!(rows[0].1.message.dedup_key.as_deref(), Some("row-0001"));
        assert_eq!(rows[0].1.message.tokens.input, 10_001);
        assert_eq!(rows[1].1.message.dedup_key.as_deref(), Some("row-0002"));
        assert_eq!(rows[3].1.message.dedup_key.as_deref(), Some("fresh-new"));
        let carried: Vec<&OpenCodeCandidate> = rows
            .iter()
            .skip(4)
            .map(|(_, candidate)| candidate)
            .collect();
        // Updates use fresh facts; row-0000 and every shared-alias even row
        // join through the alias, so none of them carry.
        assert!(carried
            .iter()
            .all(|candidate| candidate.message.dedup_key.as_deref() != Some("row-0001")));
        assert!(carried
            .iter()
            .all(|candidate| candidate.message.dedup_key.as_deref() != Some("row-0002")));
        assert!(carried
            .iter()
            .all(|candidate| candidate.message.dedup_key.as_deref() != Some("row-0000")));
        assert_eq!(
            carried
                .iter()
                .filter(|candidate| candidate.message.dedup_key.as_deref() == Some("row-0005"))
                .count(),
            2
        );
        let carried_inputs: i64 = carried
            .iter()
            .map(|candidate| candidate.message.tokens.input)
            .sum();
        let expected: i64 = (0..prior_total)
            .filter(|ordinal| {
                *ordinal == 6 || (*ordinal % 2 == 1 && *ordinal != 1 && *ordinal != 2)
            })
            .map(|ordinal| ordinal + 1)
            .sum();
        assert_eq!(carried_inputs, expected);
        // Identical carried multiplicity keeps ordinal order: the two row-0005
        // facts are adjacent in prior order.
        let positions: Vec<usize> = carried
            .iter()
            .enumerate()
            .filter(|(_, candidate)| candidate.message.dedup_key.as_deref() == Some("row-0005"))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[1] - positions[0], 1);
        // Empty fresh carries the whole prior generation; the
        // same-generation guard decodes nothing and writes nothing.
        let empty: Vec<u8> = vec![11; 32];
        carry_missing_opencode_candidates(&scratch, &empty, &previous).unwrap();
        let empty_count: i64 = scratch
            .query_row(
                "SELECT COUNT(*) FROM oc_rows WHERE source=?1",
                [&empty],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(empty_count, prior_total);
        carry_missing_opencode_candidates(&scratch, &empty, &empty).unwrap();
        let empty_again: i64 = scratch
            .query_row(
                "SELECT COUNT(*) FROM oc_rows WHERE source=?1",
                [&empty],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(empty_again, prior_total);
    }

    #[test]
    fn report_checkpoint_publication_caps_bytes_count_and_rolls_back_errors() {
        let root = TempDir::new().unwrap();
        let connection = Connection::open(root.path().join("checkpoint.sqlite3")).unwrap();
        configure_connection(&connection).unwrap();
        connection.execute_batch(
            "CREATE TEMP TABLE sq_fold_membership(position INTEGER PRIMARY KEY,membership BLOB);
             CREATE TABLE canonical_fact(value INTEGER);
             INSERT INTO canonical_fact VALUES(42);",
        ).unwrap();
        for position in 0..160_i64 {
            connection
                .execute(
                    "INSERT INTO sq_fold_membership VALUES(?1,?2)",
                    params![position, vec![position as u8; 32]],
                )
                .unwrap();
        }
        let checkpoint = ReportCheckpoint::open(&connection, [7; 32]).unwrap();
        let large_state = vec![3_u8; 900 * 1024];
        for position in 0..24 {
            checkpoint
                .save(&connection, position, &large_state)
                .unwrap();
        }
        let staged_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(length(state)+length(membership)+length(state_digest)+48),0)
             FROM report_checkpoint_staged",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(staged_bytes <= 16 * 1024 * 1024);
        assert!(
            staged_bytes >= 15 * 1024 * 1024,
            "fixture must actually reach the byte cap"
        );
        checkpoint.finish(&connection).unwrap();
        let retained = || {
            connection.query_row(
            "SELECT COUNT(*),COALESCE(SUM(length(state)+length(query_key)+length(membership)+length(state_digest)+32),0)
             FROM report_checkpoints",
            [], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        ).unwrap()
        };
        assert!(retained().1 <= 16 * 1024 * 1024);
        connection
            .execute("DELETE FROM report_checkpoint_staged", [])
            .unwrap();
        for position in 0..145_i64 {
            checkpoint.save(&connection, position, &position).unwrap();
        }
        let staged_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM report_checkpoint_staged", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(staged_count, 128, "fixture must reach the entry cap");
        checkpoint.finish(&connection).unwrap();
        assert_eq!(retained().0, 128);
        connection
            .execute("DELETE FROM report_checkpoint_staged", [])
            .unwrap();
        checkpoint
            .save(&connection, 145, &vec![0_u8; 1024 * 1024])
            .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM report_checkpoint_staged", [], |row| {
                    row.get::<_, i64>(0)
                },)
                .unwrap(),
            0,
            "oversized state must be declined before serialized allocation"
        );
        connection
            .execute_batch(
                "CREATE TRIGGER reject_checkpoint BEFORE INSERT ON report_checkpoints
             WHEN NEW.row_count=150 BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
            )
            .unwrap();
        for position in 145..152_i64 {
            checkpoint.save(&connection, position, &position).unwrap();
        }
        let before = retained();
        assert!(checkpoint.finish(&connection).is_err());
        assert_eq!(
            retained(),
            before,
            "failed publication must roll back the complete cache transaction"
        );
        assert_eq!(
            connection
                .query_row("SELECT value FROM canonical_fact", [], |row| row
                    .get::<_, i64>(0),)
                .unwrap(),
            42
        );
    }

    #[test]
    fn report_checkpoint_serializes_once_with_exact_bounded_bytes() {
        use std::io::Write;
        struct Counted<'a> {
            calls: &'a std::cell::Cell<usize>,
            value: &'a (u64, f64, Vec<String>),
        }
        impl Serialize for Counted<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                self.calls.set(self.calls.get() + 1);
                self.value.serialize(serializer)
            }
        }
        let calls = std::cell::Cell::new(0);
        let value = (
            u64::MAX,
            -0.0,
            vec!["model".to_string(), "other-model".to_string()],
        );
        let state = Counted {
            calls: &calls,
            value: &value,
        };
        let bytes = encode_report_state(&state).unwrap().unwrap();
        assert_eq!(
            calls.get(),
            1,
            "checkpoint encoding must not prewalk the aggregate"
        );
        assert_eq!(
            bytes,
            bincode::serialize(&value).unwrap(),
            "existing fixed-int/f64 codec is unchanged"
        );
        let mut buffer = ReportStateBuffer::default();
        buffer.write_all(&[7; 8]).unwrap();
        assert!(buffer.bytes.capacity() < REPORT_STATE_BYTE_LIMIT);
        assert!(buffer.write_all(&vec![0; REPORT_STATE_BYTE_LIMIT]).is_err());
        assert!(buffer.oversized);
        assert_eq!(buffer.bytes, [7; 8]);
        assert!(buffer.bytes.capacity() <= REPORT_STATE_BYTE_LIMIT);
        assert!(encode_report_state(&vec![0_u8; REPORT_STATE_BYTE_LIMIT])
            .unwrap()
            .is_none());
    }

    #[test]
    fn report_checkpoint_reads_only_longest_admissible_state() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("checkpoint.sqlite3");
        let reader = || {
            let connection = Connection::open(&path).unwrap();
            configure_connection(&connection).unwrap();
            connection.execute_batch(
                "CREATE TEMP TABLE sq_fold_membership(position INTEGER PRIMARY KEY,membership BLOB);
                 INSERT INTO sq_fold_membership VALUES(0,x'00'),(1,x'01');",
            ).unwrap();
            connection
        };
        let writer = reader();
        let saved = ReportCheckpoint::open(&writer, [42; 32]).unwrap();
        saved.save(&writer, 0, &11_u64).unwrap();
        saved.save(&writer, 1, &22_u64).unwrap();
        saved.finish(&writer).unwrap();
        writer
            .execute(
                "UPDATE report_checkpoints SET state=x'ff' WHERE row_count=0",
                [],
            )
            .unwrap();
        let current = reader();
        let restored = ReportCheckpoint::open(&current, [42; 32]).unwrap();
        assert_eq!(restored.resume_rows, 1);
        assert!(restored.has_restored());
        assert_eq!(restored.restore::<u64>().unwrap(), Some(22));
        assert!(
            !restored.has_restored(),
            "the encoded blob must not remain beside the decoded state"
        );
        assert_eq!(restored.restore::<u64>().unwrap(), None);
        writer
            .execute(
                "UPDATE report_checkpoints SET state=x'ff' WHERE row_count=1",
                [],
            )
            .unwrap();
        assert!(
            ReportCheckpoint::open(&reader(), [42; 32]).is_err(),
            "the longest state actually used for reuse still requires its checksum"
        );
    }
}
