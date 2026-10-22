use super::*;
use crate::TokenBreakdown;
use crate::UnifiedMessage;
use rusqlite::{params, OptionalExtension};
use serial_test::serial;
use std::env;
use std::ffi::OsString;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn cleanup_preserves_active_stage_and_unsafe_sidecar() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("usage");
    seed_legacy_store(&root, 3, 4);
    drop(crate::usage_store::open_database(&root).unwrap());
    let active = root.join("usage_migration_tmp_10-11.sqlite3");
    fs::copy(root.join(DATABASE), &active).unwrap();
    let writer = Connection::open(&active).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let unsafe_stage = root.join("usage_migration_tmp_10-12.sqlite3");
    fs::copy(root.join(DATABASE), &unsafe_stage).unwrap();
    let sidecar = root.join("usage_migration_tmp_10-12.sqlite3-journal");
    std::os::unix::fs::symlink(root.join(DATABASE), &sidecar).unwrap();
    let _management = crate::usage_store::management_lock(&root, true).unwrap();
    cleanup_owned_staging(&root).unwrap();
    assert!(active.exists());
    assert!(unsafe_stage.exists());
    assert!(fs::symlink_metadata(sidecar)
        .unwrap()
        .file_type()
        .is_symlink());
    writer.execute_batch("ROLLBACK").unwrap();
    drop(writer);
    cleanup_owned_staging(&root).unwrap();
    assert!(!active.exists());
}

#[test]
fn legacy_v1_copy_upgrades_without_changing_original_bytes() {
    let tmp = TempDir::new().unwrap();
    let legacy = tmp.path().join("legacy");
    let target = tmp.path().join("usage");
    seed_legacy_store(&legacy, 11, 22);
    let db = Connection::open(legacy.join(DATABASE)).unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(db);
    let before = fs::read(legacy.join(DATABASE)).unwrap();
    prepare(&target, std::slice::from_ref(&legacy)).unwrap();
    assert_eq!(before, fs::read(legacy.join(DATABASE)).unwrap());
    let migrated = Connection::open(target.join(DATABASE)).unwrap();
    assert_eq!(
        migrated
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    crate::message_cache::validate_durable_store_on(&migrated).unwrap();
}

#[cfg(unix)]
#[test]
fn orphan_cleanup_requires_exact_owned_regular_unlinked_valid_stage() {
    use std::os::unix::fs::{symlink, MetadataExt};
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("usage");
    seed_legacy_store(&root, 11, 22);
    drop(crate::usage_store::open_database(&root).unwrap());
    let canonical = root.join(DATABASE);
    let original = fs::read(&canonical).unwrap();
    let orphan = root.join("usage_migration_tmp_123-456.sqlite3");
    fs::copy(&canonical, &orphan).unwrap();
    let unknown = root.join("usage_migration_tmp_123-456-extra.sqlite3");
    fs::copy(&canonical, &unknown).unwrap();
    let linked = root.join("usage_migration_tmp_123-457.sqlite3");
    fs::hard_link(&unknown, &linked).unwrap();
    let symbolic = root.join("usage_migration_tmp_123-458.sqlite3");
    symlink(&canonical, &symbolic).unwrap();
    let invalid = root.join("usage_migration_tmp_123-459.sqlite3");
    fs::write(&invalid, b"not a store").unwrap();
    let stranded = root.join("usage_migration_tmp_123-460.sqlite3");
    fs::hard_link(&canonical, &stranded).unwrap();
    let recovery = root.join(RECOVERY);
    fs::copy(&canonical, &recovery).unwrap();
    let lock = lock_file(&root.join(LOCK)).unwrap();
    let lock_inode = lock.metadata().unwrap().ino();
    drop(lock);
    let _management = crate::usage_store::management_lock(&root, true).unwrap();
    cleanup_owned_staging(&root).unwrap();
    assert!(!orphan.exists());
    assert!(!stranded.exists());
    assert!(
        unknown.exists()
            && linked.exists()
            && symbolic
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
    );
    assert_eq!(fs::read(invalid).unwrap(), b"not a store");
    assert_eq!(fs::read(&canonical).unwrap(), original);
    assert_eq!(fs::read(&recovery).unwrap(), original);
    assert_eq!(fs::metadata(root.join(LOCK)).unwrap().ino(), lock_inode);
}

#[cfg(unix)]
#[test]
fn cleanup_preserves_partial_managed_schema_canonical_and_stage() {
    for partial_canonical in [false, true] {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("usage");
        seed_legacy_store(&root, 11, 22);
        drop(crate::usage_store::open_database(&root).unwrap());
        let canonical = root.join(DATABASE);
        let stage = root.join("usage_migration_tmp_123-456.sqlite3");
        fs::copy(&canonical, &stage).unwrap();
        let partial = if partial_canonical {
            &canonical
        } else {
            &stage
        };
        let db = Connection::open(partial).unwrap();
        db.execute_batch(
            "CREATE TABLE oc_meta(version INTEGER NOT NULL);
             INSERT INTO oc_meta VALUES(2);
             CREATE TABLE user_notes(note TEXT);
             INSERT INTO user_notes VALUES('preserve');",
        )
        .unwrap();
        // Payload admission alone cannot establish managed-schema completeness.
        validate_legacy_identity(&db).unwrap();
        crate::message_cache::validate_durable_store_on(&db).unwrap();
        drop(db);
        let canonical_before = fs::read(&canonical).unwrap();
        let stage_before = fs::read(&stage).unwrap();
        let _management = crate::usage_store::management_lock(&root, true).unwrap();
        assert_eq!(cleanup_owned_staging(&root).is_err(), partial_canonical);
        assert_eq!(fs::read(&canonical).unwrap(), canonical_before);
        assert_eq!(fs::read(&stage).unwrap(), stage_before);
    }
}

#[test]
fn prepare_existing_target_without_owned_stages_defers_database_admission() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("usage");
    seed_legacy_store(&root, 11, 22);
    drop(crate::usage_store::open_database(&root).unwrap());
    let canonical = root.join(DATABASE);
    let db = Connection::open(&canonical).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    drop(db);
    let original = fs::read(&canonical).unwrap();
    let unrelated = root.join("usage_migration_tmp_not-owned.sqlite3");
    fs::copy(&canonical, &unrelated).unwrap();
    // Without a reclaimable owned stage, prepare must leave admission to its
    // caller rather than opening or validating the existing target.
    prepare(&root, &[]).unwrap();
    assert!(crate::usage_store::open_existing_database(&root).is_err());
    assert_eq!(fs::read(&canonical).unwrap(), original);
    assert_eq!(fs::read(&unrelated).unwrap(), original);
}

#[test]
fn repeated_prepare_preserves_locked_and_partial_owned_stages() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("usage");
    seed_legacy_store(&root, 11, 22);
    drop(crate::usage_store::open_database(&root).unwrap());
    let canonical = root.join(DATABASE);
    let stage = root.join("usage_migration_tmp_123-456.sqlite3");
    fs::copy(&canonical, &stage).unwrap();
    let partial = root.join("usage_migration_tmp_123-457.sqlite3");
    fs::write(&partial, b"SQLite format 3\0partial").unwrap();
    let writer = Connection::open(&stage).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    let original = fs::read(&stage).unwrap();
    // Canonical admission remains the caller's job, including when an owned
    // stage becomes readable again. Maintenance would reject this target.
    let db = Connection::open(&canonical).unwrap();
    db.pragma_update(None, "user_version", 999).unwrap();
    drop(db);
    prepare(&root, &[]).unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    for _ in 0..3 {
        prepare(&root, &[]).unwrap();
    }
    assert_eq!(fs::read(&stage).unwrap(), original);
    assert_eq!(fs::read(&partial).unwrap(), b"SQLite format 3\0partial");
    writer.execute_batch("BEGIN EXCLUSIVE; ROLLBACK").unwrap();
    assert!(crate::usage_store::open_existing_database(&root).is_err());
}

#[cfg(unix)]
#[test]
fn cleanup_preserves_sole_stage_without_admitted_canonical() {
    use std::os::unix::fs::MetadataExt;
    for target in [
        None,
        Some(b"corrupt".as_slice()),
        Some(b"unsupported".as_slice()),
    ] {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("usage");
        seed_legacy_store(&root, 11, 22);
        drop(crate::usage_store::open_database(&root).unwrap());
        let canonical = root.join(DATABASE);
        let stage = root.join("usage_migration_tmp_123-456.sqlite3");
        fs::rename(&canonical, &stage).unwrap();
        let original = fs::read(&stage).unwrap();
        if let Some(bytes) = target {
            if bytes == b"unsupported" {
                fs::copy(&stage, &canonical).unwrap();
                let db = Connection::open(&canonical).unwrap();
                db.pragma_update(None, "user_version", 999).unwrap();
            } else {
                fs::write(&canonical, bytes).unwrap();
            }
        }
        let canonical_before = fs::read(&canonical).ok();
        let lock = lock_file(&root.join(LOCK)).unwrap();
        let lock_inode = lock.metadata().unwrap().ino();
        drop(lock);
        {
            let _management = crate::usage_store::management_lock(&root, true).unwrap();
            let result = cleanup_owned_staging(&root);
            assert_eq!(result.is_ok(), target.is_none());
        }
        assert_eq!(fs::read(&stage).unwrap(), original);
        assert_eq!(fs::read(&canonical).ok(), canonical_before);
        prepare(&root, &[]).unwrap();
        assert_eq!(fs::read(&stage).unwrap(), original);
        assert_eq!(fs::read(&canonical).ok(), canonical_before);
        assert_eq!(fs::metadata(root.join(LOCK)).unwrap().ino(), lock_inode);
        let db = Connection::open(&stage).unwrap();
        crate::message_cache::validate_durable_store_on(&db).unwrap();
    }
}

struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn capture() -> Self {
        Self(
            [
                "SYRTIS_USAGE_DIR",
                "SYRTIS_CONFIG_DIR",
                "TOKSCALE_CONFIG_DIR",
            ]
            .into_iter()
            .map(|key| (key, env::var_os(key)))
            .collect(),
        )
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, previous) in self.0.drain(..) {
                match previous {
                    Some(value) => env::set_var(key, value),
                    None => env::remove_var(key),
                }
            }
        }
    }
}

/// Build a captured context for the fixture profile: TOKSCALE_CONFIG_DIR
/// points at the fixture, so captured usage/legacy roots bind from captured
/// inputs only. Tests call `prepare(target, ctx.legacy_usage_roots())` —
/// the same shape Main's entrypoints use — instead of env-pointing.
fn captured_roots(
    legacy_cache: &Path,
    target: &Path,
) -> (EnvGuard, crate::ResolvedLocalSourceContext) {
    use crate::scanner::ScannerSettings;
    use crate::ResolvedLocalSourceContext;
    let guard = EnvGuard::capture();
    unsafe {
        env::set_var("TOKSCALE_CONFIG_DIR", legacy_cache.parent().unwrap());
        env::remove_var("SYRTIS_CONFIG_DIR");
        env::remove_var("SYRTIS_USAGE_DIR");
    }
    let old_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(target.parent().unwrap_or(target)).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(target.parent().unwrap_or(target).to_path_buf()),
        true,
        ScannerSettings::default(),
    )
    .unwrap();
    std::env::set_current_dir(&old_cwd).unwrap();
    // Force the exact dirs regardless of platform branch: the test profile
    // is explicit, so captured roots must equal these.
    assert_eq!(
        context.usage_dir(),
        legacy_cache.parent().unwrap().join("usage"),
        "TOKSCALE profile usage must be the durable root"
    );
    assert!(
        context
            .legacy_usage_roots()
            .contains(&legacy_cache.to_path_buf()),
        "captured legacy roots must contain the fixture cache: {:?}",
        context.legacy_usage_roots(),
    );
    let _ = target;
    (guard, context)
}

/// Call prepare with captured roots, overriding the durable target dir.
fn prepare_captured(
    context: &crate::ResolvedLocalSourceContext,
    target: &Path,
) -> Result<(), String> {
    prepare(target, context.legacy_usage_roots())
}

/// Minimal valid canonical store: one claude source, one generation, two
/// rows (one live, one retained-only) with a correct retained digest.
fn seed_legacy_store(dir: &Path, input_a: i64, input_b: i64) -> (Vec<u8>, Vec<u8>) {
    use crate::message_cache::{CacheIdentity, SourceFingerprint, SourceMessageCache};
    fs::create_dir_all(dir).unwrap();
    let source = dir.join("native-session.jsonl");
    fs::write(&source, b"source\n").unwrap();
    let live = live_message("live:req", input_a);
    let retained = live_message("kept:req", input_b);
    let cache = SourceMessageCache::open_from_root(Some(dir));
    cache
        .write_source(
            CacheIdentity::for_client(crate::ClientId::Claude),
            &source,
            SourceFingerprint::from_path(&source).unwrap(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.push(&live, false, false)?;
                writer.push(&retained, true, false)?;
                Ok(())
            },
        )
        .unwrap();
    fs::remove_file(source).unwrap();
    drop(cache);
    // This fixture is the pre-append-reuse schema, not a current store with
    // manually inconsistent generation-storage metadata.
    let connection = Connection::open(dir.join(DATABASE)).unwrap();
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
         DROP TABLE mc_generation_storage;
         ALTER TABLE mc_codex_turn_ids DROP COLUMN born_generation;
         UPDATE mc_meta SET value=1 WHERE key='schema_version';
         COMMIT;",
        )
        .unwrap();
    (fact_bytes(&live), fact_bytes(&retained))
}

fn live_message(key: &str, input: i64) -> UnifiedMessage {
    UnifiedMessage::new_with_dedup(
        "claude",
        "claude-3-5-sonnet",
        "openrouter",
        "session",
        100,
        TokenBreakdown {
            input,
            output: 10,
            ..Default::default()
        },
        0.0,
        Some(key.to_string()),
    )
}

fn fact_bytes(message: &UnifiedMessage) -> Vec<u8> {
    use bincode::Options;
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .serialize(message)
        .unwrap()
}

fn target_inputs(target: &Path) -> Vec<(i64, bool)> {
    let conn = Connection::open(target.join(DATABASE)).unwrap();
    let mut statement = conn
        .prepare("SELECT message,retained FROM mc_rows ORDER BY ordinal")
        .unwrap();
    statement
        .query_map([], |row| {
            let bytes: Vec<u8> = row.get(0)?;
            let retained: bool = row.get(1)?;
            let message: UnifiedMessage = {
                use bincode::Options;
                bincode::DefaultOptions::new()
                    .with_fixint_encoding()
                    .with_limit(bytes.len() as u64)
                    .deserialize(&bytes)
                    .unwrap()
            };
            Ok((message.tokens.input, retained))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn marker_of(target: &Path) -> Option<String> {
    let conn = Connection::open(target.join(DATABASE)).unwrap();
    conn.query_row(
        "SELECT value FROM mc_meta WHERE key='usage_migration_completed_source'",
        [],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

#[test]
#[serial]
fn migrates_wal_rows_and_marks_idempotent() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    // Extra committed-but-uncheckpointed WAL row: open a second connection,
    // write one more retained row, leave WAL uncheckpointed.
    seed_legacy_store(&legacy, 265, 42000);
    {
        let conn = Connection::open(legacy.join("source-message-cache.sqlite3")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        let extra = live_message("wal:req", 7);
        let bytes = fact_bytes(&extra);
        let workspace = bincode::serialize(&None::<String>).unwrap();
        conn.execute(
            "INSERT INTO mc_rows(source_id,generation,ordinal,dedup_key,retained,\
             fallback,timestamp,message,digest,session_id,model_id,client,\
             provider_id,workspace,workspace_label)\
             VALUES(1,1,2,'wal:req',1,0,101,?1,?2,'session','m','claude',\
             'openrouter',?3,?3)",
            params![bytes, sha2::Sha256::digest(&bytes).as_slice(), workspace],
        )
        .unwrap();
        // Recompute counts + digest for 3 rows / 2 retained.
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"tokscale-retained-source-v2");
        for key in ["kept:req", "wal:req"] {
            hasher.update((key.len() as u64).to_le_bytes());
            hasher.update(key.as_bytes());
            let digest: Vec<u8> = conn
                .query_row(
                    "SELECT digest FROM mc_rows WHERE dedup_key=?1",
                    [key],
                    |row| row.get(0),
                )
                .unwrap();
            hasher.update(&digest);
        }
        let digest = hasher.finalize().to_vec();
        conn.execute(
            "UPDATE mc_versions SET message_count=3,retained_count=2,retained_digest=?1",
            [digest],
        )
        .unwrap();
    }
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    // Main calls prepare(ctx.usage_dir(), ctx.legacy_usage_roots()) before
    // ingestion; tests use the same shape with the temp target dir.
    prepare_captured(&context, &target_dir).unwrap();
    let inputs = target_inputs(&target_dir);
    assert_eq!(
        inputs,
        vec![(265, false), (42000, true), (7, true)],
        "live + retained + uncheckpointed WAL rows must all transfer"
    );
    assert!(marker_of(&target_dir).is_some());
    // Idempotent: second run transfers nothing and changes nothing.
    prepare_captured(&context, &target_dir).unwrap();
    assert_eq!(target_inputs(&target_dir), inputs);
    // Source store untouched: still present with all rows.
    assert!(legacy.join("source-message-cache.sqlite3").exists());
    // Cache deletion cannot remove app data: deleting the legacy cache dir
    // leaves the durable target intact.
    fs::remove_dir_all(&legacy).unwrap();
    assert_eq!(target_inputs(&target_dir), inputs);
}

#[test]
fn backup_keeps_validated_snapshot_during_concurrent_wal_writes() {
    let legacy = TempDir::new().unwrap();
    seed_legacy_store(legacy.path(), 265, 42000);
    let writer = Connection::open(legacy.path().join(DATABASE)).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             INSERT INTO mc_meta(key,value) VALUES('snapshot-test','before');
             CREATE TABLE backup_padding(value BLOB);
             INSERT INTO backup_padding VALUES(zeroblob(1048576));",
        )
        .unwrap();
    let origin = Connection::open_with_flags(
        legacy.path().join(DATABASE),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let snapshot = validated_source_snapshot(&origin).unwrap();
    let target = TempDir::new().unwrap();
    let mut destination = Connection::open(target.path().join(DATABASE)).unwrap();
    let backup = rusqlite::backup::Backup::new(&snapshot, &mut destination).unwrap();
    assert_eq!(backup.step(1).unwrap(), rusqlite::backup::StepResult::More);
    writer
        .execute(
            "UPDATE mc_meta SET value='after' WHERE key='snapshot-test'",
            [],
        )
        .unwrap();
    step_backup(&backup).unwrap();
    drop(backup);
    let value = |conn: &Connection| -> String {
        conn.query_row(
            "SELECT value FROM mc_meta WHERE key='snapshot-test'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(value(&destination), "before");
    assert_eq!(value(&writer), "after");
    assert_eq!(
        target_inputs(target.path()),
        vec![(265, false), (42000, true)]
    );
    crate::message_cache::validate_durable_store_on(&destination).unwrap();
}

#[test]
#[serial]
fn corrupt_source_fails_closed_without_target() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(
        legacy.join("source-message-cache.sqlite3"),
        b"not a database",
    )
    .unwrap();
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    assert!(prepare_captured(&context, &target_dir).is_err());
    assert!(
        !target_dir.join("source-message-cache.sqlite3").exists(),
        "no empty-success target after corruption"
    );
    // Retry also fails closed, still no target.
    assert!(prepare_captured(&context, &target_dir).is_err());
}

#[test]
#[serial]
fn incomplete_source_fails_closed_without_target() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    seed_legacy_store(&legacy, 265, 42000);
    // Delete one row without updating counts: incomplete generation.
    {
        let conn = Connection::open(legacy.join("source-message-cache.sqlite3")).unwrap();
        conn.execute("DELETE FROM mc_rows WHERE ordinal=1", [])
            .unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    assert!(prepare_captured(&context, &target_dir).is_err());
    assert!(!target_dir.join("source-message-cache.sqlite3").exists());
}

#[test]
#[serial]
fn source_absent_is_clean_noop() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    fs::create_dir_all(&legacy).unwrap();
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    prepare_captured(&context, &target_dir).unwrap();
    assert!(
        !target_dir.join("source-message-cache.sqlite3").exists(),
        "absent source must not create a pre-marked target"
    );
}

#[test]
#[serial]
fn recovery_snapshot_migrates_when_canonical_absent() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    seed_legacy_store(&legacy, 100, 200);
    fs::rename(
        legacy.join("source-message-cache.sqlite3"),
        legacy.join("source-message-cache.recovery.sqlite3"),
    )
    .unwrap();
    // Remove WAL sidecars so the recovery snapshot stands alone.
    for extension in ["-wal", "-shm", "-journal"] {
        let mut name = legacy
            .join("source-message-cache.recovery.sqlite3")
            .into_os_string();
        name.push(extension);
        let _ = fs::remove_file(PathBuf::from(name));
    }
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    prepare_captured(&context, &target_dir).unwrap();
    assert_eq!(
        target_inputs(&target_dir),
        vec![(100, false), (200, true)],
        "recovery snapshot must carry retained history"
    );
}

#[test]
#[serial]
fn existing_target_is_never_reimported() {
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    seed_legacy_store(&legacy, 265, 42000);
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    prepare_captured(&context, &target_dir).unwrap();
    assert_eq!(target_inputs(&target_dir).len(), 2);
    // Simulate "cache overwritten histories": the legacy cache is replaced
    // by a different store. The durable target must not re-import it.
    fs::remove_file(legacy.join("source-message-cache.sqlite3")).unwrap();
    seed_legacy_store(&legacy, 1, 2);
    prepare_captured(&context, &target_dir).unwrap();
    assert_eq!(
        target_inputs(&target_dir),
        vec![(265, false), (42000, true)],
        "modern target must not silently re-import overwritten cache"
    );
}

#[test]
#[serial]
fn explicit_syrtis_profile_excludes_host_cache() {
    // Only SYRTIS_CONFIG_DIR set: captured legacy roots must be exactly the
    // explicit profile cache — never the live-host get_cache_dir().
    use crate::scanner::ScannerSettings;
    use crate::ResolvedLocalSourceContext;
    let home = TempDir::new().unwrap();
    let guard = EnvGuard::capture();
    unsafe {
        env::remove_var("TOKSCALE_CONFIG_DIR");
        env::set_var("SYRTIS_CONFIG_DIR", "/tmp/syrtis-migration-hermetic-test");
        env::remove_var("SYRTIS_USAGE_DIR");
    }
    let old_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(home.path()).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.path().to_path_buf()),
        true,
        ScannerSettings::default(),
    )
    .unwrap();
    std::env::set_current_dir(&old_cwd).unwrap();
    drop(guard);
    assert_eq!(
        context.legacy_usage_roots(),
        &[PathBuf::from("/tmp/syrtis-migration-hermetic-test/cache")],
        "explicit SYRTIS profile must not prepend the live host cache"
    );
    // Env mutation after capture cannot change captured inputs.
    unsafe {
        env::set_var("TOKSCALE_CONFIG_DIR", "/tmp/attacker-profile");
    }
    assert_eq!(
        context.legacy_usage_roots(),
        &[PathBuf::from("/tmp/syrtis-migration-hermetic-test/cache")],
    );
    unsafe {
        env::remove_var("TOKSCALE_CONFIG_DIR");
    }
}

#[test]
#[serial]
fn tokscale_only_profile_resolves_inside_profile() {
    // Many existing tests override only TOKSCALE_CONFIG_DIR: the durable
    // target must land in <profile>/usage, never the real user store.
    let guard = EnvGuard::capture();
    unsafe {
        env::remove_var("SYRTIS_USAGE_DIR");
        env::remove_var("SYRTIS_CONFIG_DIR");
        env::set_var("TOKSCALE_CONFIG_DIR", "/tmp/tokscale-only-profile-test");
    }
    assert_eq!(
        crate::paths::get_usage_dir(),
        PathBuf::from("/tmp/tokscale-only-profile-test/usage"),
    );
    assert_eq!(
        crate::paths::legacy_migration_cache_dirs(),
        vec![PathBuf::from("/tmp/tokscale-only-profile-test/cache")],
        "TOKSCALE profile cache == canonical get_cache_dir under override",
    );
    drop(guard);
}

#[test]
#[serial]
fn custom_home_resolves_inside_fake_home_with_host_env() {
    // home_dir=Some(temp_home) with real host env present: the captured
    // default usage root must stay inside the fake home, never the live
    // dirs::data_dir().
    use crate::scanner::ScannerSettings;
    use crate::ResolvedLocalSourceContext;
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let before = crate::paths::get_usage_dir();
    let guard = EnvGuard::capture();
    unsafe {
        // Simulate the hermetic case: no profile overrides at all, so the
        // default branch binds from the captured home.
        env::remove_var("SYRTIS_USAGE_DIR");
        env::remove_var("SYRTIS_CONFIG_DIR");
        env::remove_var("TOKSCALE_CONFIG_DIR");
    }
    let old_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(cwd.path()).unwrap();
    let context = ResolvedLocalSourceContext::capture(
        Some(home.path().to_path_buf()),
        true,
        ScannerSettings::default(),
    )
    .unwrap();
    std::env::set_current_dir(&old_cwd).unwrap();
    drop(guard);
    let bound = context.usage_dir().to_path_buf();
    assert!(
        bound.starts_with(home.path()),
        "captured usage root {bound:?} must stay inside fake home {:?} (host default was {before:?})",
        home.path(),
    );
    assert!(
        context
            .legacy_usage_roots()
            .iter()
            .all(|root| root.starts_with(home.path())),
        "custom-home migration must not discover the host's retained usage"
    );
    let _ = cwd;
}

#[test]
#[serial]
fn invalid_shard_only_source_fails_closed() {
    // Legacy root with no usable SQLite store and only invalid shard bytes:
    // prepare must take the shard-only path into the shared importer, which
    // fails closed — with no target created, proving the path reaches the
    // importer rather than erroring "source absent".
    let profile = TempDir::new().unwrap();
    let legacy = profile.path().join("cache");
    fs::create_dir_all(legacy.join("source-message-cache-v2").join("claude")).unwrap();
    fs::write(
        legacy
            .join("source-message-cache-v2")
            .join("claude")
            .join("shard-00.bin"),
        b"seeded shard sentinel",
    )
    .unwrap();
    let target = TempDir::new().unwrap();
    let target_dir = target.path().join("usage");
    let (_env, context) = captured_roots(&legacy, &target_dir);
    assert!(prepare_captured(&context, &target_dir).is_err());
    assert!(!target_dir.join("source-message-cache.sqlite3").exists());
}
