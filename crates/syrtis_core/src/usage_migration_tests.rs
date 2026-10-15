use super::*;
use crate::TokenBreakdown;
use crate::UnifiedMessage;
use rusqlite::{params, OptionalExtension};
use serial_test::serial;
use std::env;
use std::ffi::OsString;
use tempfile::TempDir;

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
