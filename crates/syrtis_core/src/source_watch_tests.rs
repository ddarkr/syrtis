use super::*;
use crate::{RemoteSourceContextV1, RemoteSourceScopeTokenV1};
use std::fs;
use tempfile::TempDir;

fn context(home: &Path, settings: scanner::ScannerSettings) -> ResolvedLocalSourceContext {
    let config = home.join("platform-config");
    let data = home.join("platform-data");
    let cache = home.join("owned-cache");
    let fingerprint =
        RemoteSourceContextV1::preview_fingerprint(home, &config, &data, &cache, &settings)
            .unwrap();
    RemoteSourceContextV1::new(
        home,
        config,
        data,
        cache,
        settings,
        RemoteSourceScopeTokenV1::new(fingerprint, 1),
    )
    .unwrap()
    .local()
    .clone()
}

fn options(client: &str) -> LocalParseOptions {
    LocalParseOptions {
        clients: Some(vec![client.to_owned()]),
        ..Default::default()
    }
}

fn write(path: &Path, contents: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn owns(manifest: &LocalSourceWatchManifest, path: &Path) -> bool {
    manifest.watch_roots().iter().any(|root| {
        root.source_root == path || (root.is_directory && path.starts_with(&root.source_root))
    })
}

#[test]
fn missing_root_recovery_and_known_topology_boundary() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let root = context.resolve_client_path(ClientId::Jcode).unwrap();
    let missing = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(owns(&missing, &root));
    assert!(!missing.check_known_sources().unwrap());
    fs::create_dir_all(&root).unwrap();
    assert!(missing.check_known_sources().unwrap());
    let empty = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    let session = root.join("session_a.json");
    write(&session, b"{\"messages\":[]}");
    // Known verification must not enumerate a new child. Directory hints and
    // periodic authoritative discovery are intentionally a separate boundary.
    assert!(!empty.check_known_sources().unwrap());
    let discovered = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(!discovered.check_known_sources().unwrap());
    fs::remove_file(&session).unwrap();
    assert!(discovered.check_known_sources().unwrap());
}

#[test]
fn missing_journal_creation_and_same_metadata_rewrite_invalidate() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_a.json");
    write(&session, b"{\"messages\":[]}");
    let journal = crate::sessions::jcode::jcode_journal_path(&session);
    let missing = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    write(&journal, b"aaaa\n");
    assert!(missing.check_known_sources().unwrap());
    let before = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    let modified = fs::metadata(&journal).unwrap().modified().unwrap();
    fs::write(&journal, b"bbbb\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(before.check_known_sources().unwrap());
    let after = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(!after.check_known_sources().unwrap());
    assert!(
        before.check_known_sources().unwrap(),
        "a second capture must not overwrite the first baseline"
    );
}

#[test]
fn dynamic_variant_relocation_and_absent_target_remain_owned() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let variant = home.path().join(".cc-mirror/variant/variant.json");
    let config = home.path().join("relocated-a");
    write(
        &variant,
        serde_json::json!({"configDir": config})
            .to_string()
            .as_bytes(),
    );
    let first = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    assert!(owns(&first, &config.join("projects")));
    assert!(!first.check_known_sources().unwrap());
    let next = home.path().join("relocated-b");
    fs::write(&variant, serde_json::json!({"configDir": next}).to_string()).unwrap();
    assert!(first.check_known_sources().unwrap());
    let second = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    assert!(owns(&second, &next.join("projects")));
    assert!(!second.check_known_sources().unwrap());
    fs::create_dir_all(next.join("projects")).unwrap();
    assert!(second.check_known_sources().unwrap());
}

#[test]
fn database_wal_changes_and_readable_empty_wal_equivalence() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let database = context.resolve_client_path(ClientId::Zcode).unwrap();
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let producer = Connection::open(&database).unwrap();
    producer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE usage(value INTEGER); INSERT INTO usage VALUES(1);").unwrap();
    let baseline = capture_local_source_watch_manifest(&context, &options("zcode")).unwrap();
    assert!(owns(&baseline, &database));
    assert!(owns(&baseline, &suffix(&database, "-wal")));
    assert!(!baseline.check_known_sources().unwrap());
    producer.execute("INSERT INTO usage VALUES(2)", []).unwrap();
    assert!(baseline.check_known_sources().unwrap());
    drop(producer);
    let absent = capture_local_source_watch_manifest(&context, &options("zcode")).unwrap();
    let wal = suffix(&database, "-wal");
    fs::write(&wal, b"").unwrap();
    assert!(!absent.check_known_sources().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o0)).unwrap();
        assert!(absent.check_known_sources().is_err());
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::remove_file(wal).unwrap();
    assert!(!absent.check_known_sources().unwrap());
}

#[test]
fn opencode_directories_do_not_block_database_watch_capture() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let root = context
        .resolve_client_root(crate::PathRoot::XdgData)
        .unwrap()
        .join("opencode");
    for name in ["log", "storage", "opencode-skipped.db"] {
        fs::create_dir_all(root.join(name)).unwrap();
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("log"), root.join("log-link")).unwrap();
    let database = root.join("opencode.db");
    let producer = Connection::open(&database).unwrap();
    producer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE usage(value INTEGER); INSERT INTO usage VALUES(1);").unwrap();
    let scan =
        scanner::scan_all_clients_with_source_context(&context, &["opencode".to_owned()]).unwrap();
    assert_eq!(scan.opencode_dbs, vec![database.clone()]);
    let baseline = capture_local_source_watch_manifest(&context, &options("opencode")).unwrap();
    assert!(owns(&baseline, &database));
    assert!(owns(&baseline, &suffix(&database, "-wal")));
    assert!(!baseline.check_known_sources().unwrap());
    producer.execute("INSERT INTO usage VALUES(2)", []).unwrap();
    assert!(baseline.check_known_sources().unwrap());
    let current = capture_local_source_watch_manifest(&context, &options("opencode")).unwrap();
    assert!(!current.check_known_sources().unwrap());
}

#[cfg(unix)]
#[test]
fn sqlite_watch_sampling_accepts_symlinked_database_and_wal() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let home_path = fs::canonicalize(home.path()).unwrap();
    let context = context(&home_path, Default::default());
    let database = home_path.join("actual.sqlite");
    let producer = Connection::open(&database).unwrap();
    producer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE usage(value INTEGER); INSERT INTO usage VALUES(1);").unwrap();
    let root = context
        .resolve_client_root(crate::PathRoot::XdgData)
        .unwrap()
        .join("opencode");
    fs::create_dir_all(&root).unwrap();
    let alias = root.join("opencode.db");
    symlink(&database, &alias).unwrap();
    symlink(suffix(&database, "-wal"), suffix(&alias, "-wal")).unwrap();
    let baseline = capture_local_source_watch_manifest(&context, &options("opencode")).unwrap();
    assert!(owns(&baseline, &database));
    assert!(!baseline.check_known_sources().unwrap());
    producer.execute("INSERT INTO usage VALUES(2)", []).unwrap();
    assert!(baseline.check_known_sources().unwrap());
    let current = capture_local_source_watch_manifest(&context, &options("opencode")).unwrap();
    assert!(!current.check_known_sources().unwrap());
}

#[cfg(unix)]
#[test]
fn unreadable_is_not_absent_even_for_empty_dependencies() {
    use std::os::unix::fs::PermissionsExt;
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_a.json");
    write(&session, b"{}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    let journal = crate::sessions::jcode::jcode_journal_path(&session);
    write(&journal, b"");
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o0)).unwrap();
    assert!(baseline.check_known_sources().is_err());
    assert!(capture_local_source_watch_manifest(&context, &options("jcode")).is_err());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(baseline.check_known_sources().unwrap());
}

#[cfg(unix)]
#[test]
fn symlink_recovery_aliases_are_preserved() {
    use std::os::unix::fs::symlink;
    let home = TempDir::new().unwrap();
    let raw = fs::canonicalize(home.path()).unwrap().join("sessions");
    let declared = home.path().join("declared");
    fs::create_dir_all(&raw).unwrap();
    symlink(&raw, &declared).unwrap();
    let mut settings = scanner::ScannerSettings::default();
    // The environment-independent fixture is confined; approve both aliases.
    settings
        .extra_scan_paths
        .insert("jcode".into(), vec![declared.clone(), raw.clone()]);
    let context = context(home.path(), settings);
    let session = raw.join("session_a.json");
    write(&session, b"{}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(owns(&baseline, &declared));
    assert!(owns(&baseline, &raw));
    assert!(!baseline.check_known_sources().unwrap());
    fs::write(&session, b"{ }").unwrap();
    assert!(baseline.check_known_sources().unwrap());
}

#[cfg(unix)]
#[test]
fn native_non_utf8_missing_root_preserves_identity_and_recovers_where_supported() {
    use std::os::unix::ffi::OsStringExt;
    let home = TempDir::new().unwrap();
    let raw = fs::canonicalize(home.path())
        .unwrap()
        .join(std::ffi::OsString::from_vec(b"sessions-\xff".to_vec()));
    let mut settings = scanner::ScannerSettings::default();
    settings
        .extra_scan_paths
        .insert("jcode".into(), vec![raw.clone()]);
    let context = context(home.path(), settings);
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(
        owns(&baseline, &raw),
        "missing native root must not be lossy-converted"
    );
    assert!(!baseline.check_known_sources().unwrap());
    // macOS APFS rejects creation with EILSEQ. Missing-root identity is still
    // required; physical non-UTF8 source recovery is exercised on Unix hosts
    // whose native filesystem accepts these names.
    match fs::create_dir_all(&raw) {
        Ok(()) => {
            write(&raw.join("session_a.json"), b"{}");
            assert!(baseline.check_known_sources().unwrap());
            let current = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
            assert!(owns(&current, &raw));
            assert!(!current.check_known_sources().unwrap());
        }
        Err(error) => {
            assert_eq!(
                error.raw_os_error(),
                Some(libc::EILSEQ),
                "unexpected filesystem failure"
            );
        }
    }
}

#[test]
fn metadata_spills_while_roots_and_retained_heap_stay_bounded() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let root = context.resolve_client_path(ClientId::Jcode).unwrap();
    fs::create_dir_all(&root).unwrap();
    let small = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    for index in 0..1200 {
        write(&root.join(format!("session_{index:04}.json")), b"{}");
    }
    let large = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert_eq!(small.watch_roots().len(), large.watch_roots().len());
    assert!(large.retained_bytes() < 1024 * 1024);
    let store = match large.known.lock() {
        Ok(store) => store,
        Err(_) => panic!("manifest lock poisoned"),
    };
    let rows: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM temp.watch_known", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        rows >= 2400,
        "each source and absent journal has immutable metadata"
    );
    let temp_store: i64 = store
        .db
        .pragma_query_value(None, "temp_store", |row| row.get(0))
        .unwrap();
    assert_eq!(temp_store, 1);
    drop(store);
    fs::write(root.join("session_1199.json"), b"{ }").unwrap();
    assert!(large.check_known_sources().unwrap());
}

#[test]
fn every_specialized_file_parser_keeps_its_effective_siblings() {
    for client in [
        ClientId::Claude,
        ClientId::Droid,
        ClientId::Kimi,
        ClientId::RooCode,
        ClientId::KiloCode,
        ClientId::Cline,
        ClientId::Grok,
        ClientId::Kiro,
    ] {
        let home = TempDir::new().unwrap();
        let context = context(home.path(), Default::default());
        let root = context.resolve_client_path(client).unwrap();
        let (source, dependency) = match client {
            ClientId::Claude => {
                let source = root.join("project/agent_child.jsonl");
                let dependency = source.with_file_name("agent_child.meta.json");
                (source, dependency)
            }
            ClientId::Droid => {
                let source = root.join("session.settings.json");
                let dependency = crate::sessions::droid::droid_jsonl_path(&source).unwrap();
                (source, dependency)
            }
            ClientId::Kimi => {
                let source = root.join("group/session/wire.jsonl");
                let dependency = crate::sessions::kimi::kimi_config_path(&source).unwrap();
                (source, dependency)
            }
            ClientId::RooCode | ClientId::KiloCode | ClientId::Cline => {
                let source = root.join("task/ui_messages.json");
                let dependency = crate::sessions::roocode::history_path_for_ui_messages(&source);
                (source, dependency)
            }
            ClientId::Grok => {
                let source = root.join("session/updates.jsonl");
                let dependency = source.with_file_name("signals.json");
                (source, dependency)
            }
            ClientId::Kiro => {
                let source = home
                    .path()
                    .join(".kiro/sessions/workspace/sess_02f1c107/session.json");
                let dependency =
                    crate::sessions::kiro::kiro_related_messages_path(&source).unwrap();
                (source, dependency)
            }
            _ => unreachable!(),
        };
        write(&source, b"{}\n");
        let baseline =
            capture_local_source_watch_manifest(&context, &options(client.as_str())).unwrap();
        assert!(
            owns(&baseline, &dependency),
            "missing dependency ownership for {}",
            client.as_str()
        );
        assert!(!baseline.check_known_sources().unwrap());
        write(&dependency, b"{ }\n");
        assert!(
            baseline.check_known_sources().unwrap(),
            "sibling-only change missed for {}",
            client.as_str()
        );
    }
}

#[test]
fn claude_sidechain_parent_and_copilot_missing_event_recover_without_primary_writes() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let projects = context.resolve_client_path(ClientId::Claude).unwrap();
    let child = projects.join("project/parent-session/subagents/agent-child.jsonl");
    write(
        &child,
        b"{\"type\":\"user\",\"isSidechain\":true,\"sessionId\":\"parent-session\"}\n",
    );
    let parent = crate::sessions::claudecode::parent_session_paths_for_cache(&child)
        .into_iter()
        .next()
        .unwrap();
    let baseline = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    write(&parent, b"{}\n");
    assert!(baseline.check_known_sources().unwrap());

    let database = home.path().join(".copilot/data.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let producer = Connection::open(database).unwrap();
    producer
        .execute_batch("CREATE TABLE usage(value INTEGER);")
        .unwrap();
    let event = home
        .path()
        .join(".copilot/session-state/known/events.jsonl");
    fs::create_dir_all(event.parent().unwrap()).unwrap();
    let baseline = capture_local_source_watch_manifest(&context, &options("copilot")).unwrap();
    assert!(owns(&baseline, &event));
    write(&event, b"{}\n");
    assert!(baseline.check_known_sources().unwrap());
}

#[test]
fn cache_named_source_inputs_are_not_blanket_ignored() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("cache/session_a.json");
    write(&session, b"{}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(!baseline.ignored_paths().contains(&session));
    fs::write(&session, b"{ }").unwrap();
    assert!(baseline.check_known_sources().unwrap());
    // Canonical store writes are deliberately excluded from source dirt.
    let store = crate::usage_store::open_database(context.usage_dir()).unwrap();
    store.execute_batch("CREATE TABLE manifest_owned_output_probe(value INTEGER); INSERT INTO manifest_owned_output_probe VALUES(1);").unwrap();
    let fresh = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    store
        .execute("INSERT INTO manifest_owned_output_probe VALUES(2)", [])
        .unwrap();
    assert!(!fresh.check_known_sources().unwrap());
}

#[test]
fn retained_history_updates_dirty_claude_but_not_unrelated_source_scopes() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let source = context
        .resolve_client_path(ClientId::Claude)
        .unwrap()
        .join("project/session.jsonl");
    let row = |id: &str| {
        serde_json::json!({
        "type": "assistant", "timestamp": "2026-01-01T00:00:00Z", "requestId": id,
        "message": { "id": id, "model": "claude-sonnet-4", "usage": { "input_tokens": 10, "output_tokens": 3 } }
    }).to_string() + "\n"
    };
    let first = row("message-1");
    let second = row("message-2");
    write(&source, (first + &second).as_bytes());
    crate::synchronize_local_usage(
        &context,
        options("claude"),
        crate::LocalUsageSelection::LocalClients,
    )
    .unwrap();
    fs::write(&source, second).unwrap();
    let claude_before_retention =
        capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    let unrelated = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    crate::synchronize_local_usage(
        &context,
        options("claude"),
        crate::LocalUsageSelection::LocalClients,
    )
    .unwrap();
    assert!(
        claude_before_retention.check_known_sources().unwrap(),
        "retained publication must invalidate despite byte-identical live sources"
    );
    assert!(!unrelated.check_known_sources().unwrap());
    let fresh = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    assert!(!fresh.check_known_sources().unwrap());
}

#[cfg(unix)]
#[test]
fn replaced_store_rejects_manifest_instead_of_reusing_private_metadata() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    let cache = context.usage_dir();
    let path = cache.join("source-message-cache.sqlite3");
    fs::rename(&path, cache.join("preserved.sqlite3")).unwrap();
    assert!(baseline.check_known_sources().is_err());
}

#[test]
fn excluded_dynamic_missing_tasks_do_not_become_source_ownership() {
    let home = TempDir::new().unwrap();
    let relocated = home.path().join("relocated/projects");
    let variant = home.path().join(".cc-mirror/variant/variant.json");
    write(
        &variant,
        serde_json::json!({"configDir": relocated.parent().unwrap()})
            .to_string()
            .as_bytes(),
    );
    let mut settings = scanner::ScannerSettings::default();
    settings
        .excluded_scan_paths
        .insert("claude".into(), vec![relocated.clone()]);
    let context = context(home.path(), settings);
    let baseline = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    assert!(!owns(&baseline, &relocated));
    fs::create_dir_all(&relocated).unwrap();
    let recovered = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    assert!(!owns(&recovered, &relocated));
    assert!(!recovered.check_known_sources().unwrap());
}

#[test]
fn vfs_probe_does_not_drop_producer_locks() {
    const CHILD_PATH: &str = "TOKSCALE_WATCH_LOCK_CHILD";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let connection = Connection::open(PathBuf::from(path)).unwrap();
        connection
            .busy_timeout(std::time::Duration::from_millis(20))
            .unwrap();
        assert!(
            connection.execute_batch("BEGIN IMMEDIATE").is_err(),
            "manifest probe released another connection's producer lock"
        );
        return;
    }
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let database = context.resolve_client_path(ClientId::Zcode).unwrap();
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let producer = Connection::open(&database).unwrap();
    producer
        .execute_batch("CREATE TABLE usage(value INTEGER); BEGIN IMMEDIATE;")
        .unwrap();
    let baseline = capture_local_source_watch_manifest(&context, &options("zcode")).unwrap();
    assert!(!baseline.check_known_sources().unwrap());
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("source_watch::tests::vfs_probe_does_not_drop_producer_locks")
        .arg("--nocapture")
        .env(CHILD_PATH, &database)
        .status()
        .unwrap();
    assert!(child.success());
    producer.execute_batch("ROLLBACK").unwrap();
}

fn changed_paths(value: LocalSourceVerification) -> (Vec<PathBuf>, Arc<LocalSourceWatchManifest>) {
    match value {
        LocalSourceVerification::Changed { paths, manifest } => (paths, manifest),
        _ => panic!("expected Changed plan"),
    }
}

#[test]
fn verify_changes_collects_file_change_with_pre_ingestion_baseline() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_a.json");
    write(&session, b"{\"a\":1}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(matches!(
        baseline.verify_changes().unwrap(),
        LocalSourceVerification::Unchanged
    ));
    // Same size and mtime, different sampled content: metadata alone misses it.
    let modified = fs::metadata(&session).unwrap().modified().unwrap();
    fs::write(&session, b"{\"a\":2}").unwrap();
    fs::File::options()
        .write(true)
        .open(&session)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert!(baseline.check_known_sources().unwrap());
    let (paths, replacement) = changed_paths(baseline.verify_changes().unwrap());
    assert_eq!(paths, vec![session.clone()]);
    // Replacement already holds pre-ingestion observations: still clean, and a
    // late write after the read dirties it for the next check.
    assert!(!replacement.check_known_sources().unwrap());
    assert!(matches!(
        replacement.verify_changes().unwrap(),
        LocalSourceVerification::Unchanged
    ));
    fs::write(&session, b"{\"messages\":[\"late write\"]}").unwrap();
    assert!(replacement.check_known_sources().unwrap());
    // The immutable original never advanced past the first change.
    assert!(baseline.check_known_sources().unwrap());
}

#[test]
fn verify_changes_rediscovers_topology_new_delete_replace_and_wal() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let missing = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    // New directory children are periodic-discovery scope, not known-file dirt.
    fs::create_dir_all(context.resolve_client_path(ClientId::Jcode).unwrap()).unwrap();
    assert!(matches!(
        missing.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_b.json");
    write(&session, b"{}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    // Nested new files need explicit topology discovery; metadata alone must
    // not claim detection, so only known probes participate.
    fs::write(
        context
            .resolve_client_path(ClientId::Jcode)
            .unwrap()
            .join("unseen_child.json"),
        b"{}",
    )
    .unwrap();
    assert!(matches!(
        baseline.verify_changes().unwrap(),
        LocalSourceVerification::Unchanged
    ));
    fs::remove_file(&session).unwrap();
    assert!(matches!(
        baseline.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
    write(&session, b"{}");
    let restored = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    let old_session = session.with_extension("preserved");
    fs::rename(&session, &old_session).unwrap();
    write(&session, b"{ }");
    assert!(matches!(
        restored.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
    let database = context.resolve_client_path(ClientId::Zcode).unwrap();
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    let producer = Connection::open(&database).unwrap();
    producer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE usage(value INTEGER); INSERT INTO usage VALUES(1);").unwrap();
    let watched = capture_local_source_watch_manifest(&context, &options("zcode")).unwrap();
    producer.execute("INSERT INTO usage VALUES(2)", []).unwrap();
    assert!(matches!(
        watched.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
}

#[test]
fn verify_changes_rediscovers_retained_drift_and_overflows_bounded_plan() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_a.json");
    write(&session, b"{}");
    for index in 0..257 {
        write(
            &context
                .resolve_client_path(ClientId::Jcode)
                .unwrap()
                .join(format!("session_overflow_{index:03}.json")),
            b"{}",
        );
    }
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    for index in 0..257 {
        fs::write(
            context
                .resolve_client_path(ClientId::Jcode)
                .unwrap()
                .join(format!("session_overflow_{index:03}.json")),
            b"{ }",
        )
        .unwrap();
    }
    assert!(baseline.check_known_sources().unwrap());
    assert!(matches!(
        baseline.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
    let clean = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    // Retained publication invalidates despite byte-identical live sources.
    let source = context
        .resolve_client_path(ClientId::Claude)
        .unwrap()
        .join("project/session.jsonl");
    let row = |id: &str| {
        serde_json::json!({
        "type": "assistant", "timestamp": "2026-01-01T00:00:00Z", "requestId": id,
        "message": { "id": id, "model": "claude-sonnet-4", "usage": { "input_tokens": 10, "output_tokens": 3 } }
    })
        .to_string()
            + "\n"
    };
    write(&source, (row("message-1") + &row("message-2")).as_bytes());
    crate::synchronize_local_usage(
        &context,
        options("claude"),
        crate::LocalUsageSelection::LocalClients,
    )
    .unwrap();
    fs::write(&source, row("message-2")).unwrap();
    let claude_before = capture_local_source_watch_manifest(&context, &options("claude")).unwrap();
    crate::synchronize_local_usage(
        &context,
        options("claude"),
        crate::LocalUsageSelection::LocalClients,
    )
    .unwrap();
    assert!(matches!(
        claude_before.verify_changes().unwrap(),
        LocalSourceVerification::Rediscover
    ));
    assert!(matches!(
        clean.verify_changes().unwrap(),
        LocalSourceVerification::Unchanged
    ));
    assert!(clean.retained_bytes() < 1024 * 1024);
}

#[test]
fn verify_changes_rejects_unreadable_store_explicitly() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let session = context
        .resolve_client_path(ClientId::Jcode)
        .unwrap()
        .join("session_a.json");
    write(&session, b"{}");
    let baseline = capture_local_source_watch_manifest(&context, &options("jcode")).unwrap();
    assert!(matches!(
        baseline.verify_changes().unwrap(),
        LocalSourceVerification::Unchanged
    ));
    let cache = context.usage_dir();
    fs::rename(
        cache.join("source-message-cache.sqlite3"),
        cache.join("preserved.sqlite3"),
    )
    .unwrap();
    assert!(baseline.verify_changes().is_err());
}

fn incremental_claude_row(id: &str, input: i64) -> String {
    serde_json::json!({
        "type":"assistant","timestamp":"2026-01-01T00:00:00Z","requestId":id,
        "message":{"id":id,"model":"claude-sonnet-4",
            "usage":{"input_tokens":input,"output_tokens":3}}
    })
    .to_string()
        + "\n"
}

fn incremental_codex_token(total: i64) -> String {
    serde_json::json!({
        "type":"event_msg","timestamp":"2026-01-01T00:00:00Z",
        "payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":total*5,"output_tokens":total*2},
            "last_token_usage":{"input_tokens":5,"output_tokens":2}
        }}
    })
    .to_string()
        + "\n"
}

fn incremental_rows(snapshot: &crate::LocalUsageSnapshot) -> Vec<crate::UnifiedMessage> {
    let mut rows = Vec::new();
    crate::visit_local_usage_snapshot(
        snapshot,
        crate::LocalUsageQuery::default(),
        &|_| true,
        &mut |message| rows.push(message.clone()),
    )
    .unwrap();
    rows
}

#[test]
fn targeted_sources_match_full_ingestion_with_duplicates_and_multiple_changes() {
    for mode in [
        crate::LocalUsageSelection::Report,
        crate::LocalUsageSelection::LocalClients,
    ] {
        let home = TempDir::new().unwrap();
        let context = context(home.path(), Default::default());
        let claude = context
            .resolve_client_path(ClientId::Claude)
            .unwrap()
            .join("project/session.jsonl");
        let duplicate = claude.with_file_name("duplicate.jsonl");
        let codex = context
            .resolve_client_path(ClientId::Codex)
            .unwrap()
            .join("2026/01/01/rollout-proof.jsonl");
        let initial_claude = incremental_claude_row("first", 10);
        write(&claude, initial_claude.as_bytes());
        write(&duplicate, initial_claude.as_bytes());
        let initial_codex = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"proof\",\"source\":\"cli\",\"cwd\":\"/fixture\",\"model_provider\":\"openai\"}}\n",
            "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-5.5\"}}\n"
        ).to_string() + &incremental_codex_token(1);
        write(&codex, initial_codex.as_bytes());
        let options = LocalParseOptions {
            clients: Some(vec!["claude".into(), "codex".into()]),
            ..Default::default()
        };
        let initial = crate::synchronize_local_usage(&context, options.clone(), mode).unwrap();
        assert_eq!(
            incremental_rows(&initial)
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            20
        );
        fs::write(
            &claude,
            initial_claude + &incremental_claude_row("second", 20),
        )
        .unwrap();
        fs::write(&codex, initial_codex + &incremental_codex_token(2)).unwrap();
        let targeted = crate::try_synchronize_local_usage_sources(
            &context,
            options.clone(),
            mode,
            &initial,
            &[claude.clone(), codex.clone()],
        )
        .unwrap()
        .expect("existing primary sources are eligible");
        let observed = incremental_rows(&targeted);
        assert_eq!(
            observed
                .iter()
                .map(|message| message.tokens.total())
                .sum::<i64>(),
            50
        );
        let full = crate::synchronize_local_usage(&context, options.clone(), mode).unwrap();
        assert_eq!(
            serde_json::to_value(&observed).unwrap(),
            serde_json::to_value(incremental_rows(&full)).unwrap()
        );
        assert!(
            crate::try_synchronize_local_usage_sources(
                &context,
                options.clone(),
                mode,
                &initial,
                std::slice::from_ref(&claude),
            )
            .unwrap()
            .is_none(),
            "superseded base must not be revived"
        );
        assert!(
            crate::try_synchronize_local_usage_sources(
                &context,
                options.clone(),
                mode,
                &full,
                &[claude.with_extension("meta.json")],
            )
            .unwrap()
            .is_none(),
            "a sibling is not a primary source"
        );
        assert!(
            crate::try_synchronize_local_usage_sources(
                &context,
                self::options("claude"),
                mode,
                &full,
                std::slice::from_ref(&claude),
            )
            .unwrap()
            .is_none(),
            "changed producer scope must rediscover"
        );
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&codex)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        assert!(crate::try_synchronize_local_usage_sources(
            &context,
            options.clone(),
            mode,
            &full,
            &[codex],
        )
        .is_err());
        assert_eq!(
            serde_json::to_value(&observed).unwrap(),
            serde_json::to_value(incremental_rows(&full)).unwrap(),
            "failed parse keeps the last completed scope readable"
        );
    }
}

#[test]
fn targeted_claude_corrections_and_truncation_preserve_collected_usage() {
    let home = TempDir::new().unwrap();
    let context = context(home.path(), Default::default());
    let source = context
        .resolve_client_path(ClientId::Claude)
        .unwrap()
        .join("project/session.jsonl");
    write(
        &source,
        (incremental_claude_row("first", 10) + &incremental_claude_row("second", 20)).as_bytes(),
    );
    let mode = crate::LocalUsageSelection::Report;
    let initial = crate::synchronize_local_usage(&context, options("claude"), mode).unwrap();
    fs::write(&source, incremental_claude_row("second", 40)).unwrap();
    let targeted = crate::try_synchronize_local_usage_sources(
        &context,
        options("claude"),
        mode,
        &initial,
        std::slice::from_ref(&source),
    )
    .unwrap()
    .unwrap();
    let observed = incremental_rows(&targeted);
    assert_eq!(
        observed
            .iter()
            .map(|message| message.tokens.total())
            .sum::<i64>(),
        56
    );
    let full = crate::synchronize_local_usage(&context, options("claude"), mode).unwrap();
    assert_eq!(
        serde_json::to_value(&observed).unwrap(),
        serde_json::to_value(incremental_rows(&full)).unwrap()
    );
    fs::remove_file(source).unwrap();
    let retained = crate::synchronize_local_usage(&context, options("claude"), mode).unwrap();
    assert_eq!(
        incremental_rows(&retained)
            .iter()
            .map(|message| message.tokens.total())
            .sum::<i64>(),
        56
    );
}
