use super::*;
use crate::TokenBreakdown;
use tempfile::TempDir;

fn message(key: &str, input: i64) -> UnifiedMessage {
    split_message(key, input, false, 0)
}

/// A Claude row with explicit cache-split provenance: `input` is the uncached
/// remainder, `cache_read` the separately reported cached bucket. Same shape
/// the parser emits for `{"input_tokens":265,"cache_read_input_tokens":42000}`.
fn split_message(key: &str, input: i64, split: bool, cache_read: i64) -> UnifiedMessage {
    let mut message = UnifiedMessage::new_with_dedup(
        "claude",
        "claude-3-5-sonnet",
        "openrouter",
        "session",
        100,
        TokenBreakdown {
            input,
            output: 10,
            cache_read,
            ..Default::default()
        },
        0.0,
        Some(key.to_string()),
    );
    message.input_tokens_are_cache_split = split;
    message
}

fn source(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, b"source\n").unwrap();
    path
}

fn publish(
    cache: &SourceMessageCache,
    identity: CacheIdentity,
    path: &Path,
    messages: &[UnifiedMessage],
) -> CachedSourceMetadata {
    let observed = cache.retention_metadata(identity, path).unwrap();
    cache
        .write_source(
            identity,
            path,
            SourceFingerprint::from_path(path).unwrap(),
            observed.as_ref(),
            None,
            None,
            &mut |message| {
                crate::sessions::claudecode::refresh_retained_message_context(
                    message, path, None, None,
                )
            },
            &mut |_, _| Ok(()),
            |writer| {
                for message in messages {
                    writer.push(message, false, false)?;
                }
                Ok(())
            },
        )
        .unwrap()
}

fn rows(
    cache: &SourceMessageCache,
    metadata: &CachedSourceMetadata,
) -> Vec<(UnifiedMessage, bool, bool)> {
    let mut messages = Vec::new();
    cache
        .visit_version(
            metadata.source_id,
            metadata.generation,
            &mut |message: &UnifiedMessage, retained, fallback| {
                messages.push((message.clone(), retained, fallback));
                Ok(())
            },
        )
        .unwrap();
    messages
}

fn seed_legacy(
    root: &Path,
    identity: CacheIdentity,
    path: &Path,
    entries: &[CachedSourceEntry],
) -> PathBuf {
    let shard = root
        .join(CACHE_SHARD_DIRNAME)
        .join(identity.namespace)
        .join(format!(
            "shard-{:02x}.bin",
            legacy_shard_index(identity.namespace, &CachedPath::from_path(path))
        ));
    fs::create_dir_all(shard.parent().unwrap()).unwrap();
    let envelope = CachedShardEnvelope {
        format_version: CACHE_FORMAT_VERSION,
        parser_namespace: identity.namespace.to_string(),
        parser_version: identity.parser_version,
        payload: encode(entries).unwrap(),
    };
    fs::write(&shard, encode(&envelope).unwrap()).unwrap();
    shard
}

#[test]
fn sqlite_empty_wal_normalization_keeps_readability_and_primary_validation() {
    let root = TempDir::new().unwrap();
    let path = source(root.path(), "source.db");
    let wal = append_path_suffix(&path, "-wal");
    let absent = SourceFingerprint::from_sqlite_path(&path).unwrap();
    let copilot_absent = SourceFingerprint::from_copilot_desktop_path(&path).unwrap();
    let generic_absent =
        RelatedFileFingerprint::from_path("-wal".into(), &wal, ContentHashMode::SamplesOnly)
            .unwrap();
    fs::write(&wal, b"").unwrap();
    assert_eq!(SourceFingerprint::from_sqlite_path(&path).unwrap(), absent);
    assert_eq!(
        SourceFingerprint::from_copilot_desktop_path(&path).unwrap(),
        copilot_absent
    );
    assert!(fingerprint_is_current(&path, &absent));
    assert_ne!(
        RelatedFileFingerprint::from_path("-wal".into(), &wal, ContentHashMode::SamplesOnly)
            .unwrap(),
        generic_absent,
        "generic siblings keep exact presence",
    );
    #[cfg(unix)]
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::metadata(&wal).unwrap().permissions();
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o000)).unwrap();
        let sqlite = SourceFingerprint::from_sqlite_path(&path);
        let copilot = SourceFingerprint::from_copilot_desktop_path(&path);
        let current = fingerprint_is_current(&path, &absent);
        fs::set_permissions(&wal, permissions).unwrap();
        assert!(
            sqlite.is_none() && copilot.is_none() && !current,
            "unreadable empty WAL is never absent"
        );
    }
    fs::write(&wal, b"uncheckpointed-frame").unwrap();
    assert_ne!(SourceFingerprint::from_sqlite_path(&path).unwrap(), absent);
    assert!(!fingerprint_is_current(&path, &absent));
    fs::remove_file(&wal).unwrap();
    assert_eq!(SourceFingerprint::from_sqlite_path(&path).unwrap(), absent);
    assert!(fingerprint_is_current(&path, &absent));
    fs::write(&path, b"checkpointed-main-db").unwrap();
    assert!(!fingerprint_is_current(&path, &absent));
    fs::create_dir(&wal).unwrap();
    assert!(SourceFingerprint::from_sqlite_path(&path).is_none());
}

#[test]
fn overlapping_cold_and_warm_openers_admit_one_schema_without_rewriting_it() {
    let root = TempDir::new().unwrap();
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let run = || {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let barrier = barrier.clone();
                let root = root.path().to_path_buf();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..8 {
                        let cache = SourceMessageCache::open_from_root(Some(&root));
                        cache.ensure_open().unwrap();
                        assert!(cache
                            .metadata(identity, &root.join("absent.jsonl"))
                            .unwrap()
                            .is_none());
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    };
    run();
    run();
    let connection = crate::usage_store::open_database(root.path()).unwrap();
    let before = connection.total_changes();
    initialize(&connection).unwrap();
    assert_eq!(
        connection.total_changes(),
        before,
        "warm schema admission is read-only"
    );
}

#[test]
fn metadata_and_warm_visits_are_source_keyed_and_read_only() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let one = source(sources.path(), "one.jsonl");
    let two = source(sources.path(), "two.jsonl");
    let mut first = message("message:req", 10);
    first.workspace_key = Some("workspace".to_string());
    first.workspace_label = Some("label".to_string());
    let good = publish(&cache, identity, &one, std::slice::from_ref(&first));
    let bad = publish(&cache, identity, &two, &[message("other:req", 20)]);
    cache
        .with_database(None, |db| {
            db.execute(
                "UPDATE mc_rows SET message=x'ff' WHERE source_id=?1 AND generation=?2",
                params![bad.source_id, bad.generation],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    let changes = || {
        cache
            .with_database(None, |db| {
                db.query_row("SELECT total_changes()", [], |row| row.get::<_, u64>(0))
                    .map_err(sql_error)
            })
            .unwrap()
    };
    let before = changes();
    for _ in 0..3 {
        assert_eq!(
            cache.metadata(identity, &one).unwrap().unwrap().generation,
            good.generation
        );
        assert_eq!(
            rows(&cache, &good)[0].0.workspace_key.as_deref(),
            Some("workspace")
        );
        cache
            .visit_rows(
                identity,
                &one,
                Some(good.generation),
                &mut |message: &UnifiedMessage, _, _| {
                    assert_eq!(message.tokens.input, 10);
                    Ok(())
                },
            )
            .unwrap();
    }
    assert_eq!(
        changes(),
        before,
        "warm source metadata/row visits never rewrite ingestion facts"
    );
    cache.with_database(None,|db| {
        let (payload,workspace,label): (Vec<u8>,Vec<u8>,Vec<u8>) = db.query_row(
            "SELECT message,workspace,workspace_label FROM mc_rows WHERE source_id=?1 AND generation=?2",
            params![good.source_id,good.generation],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).map_err(sql_error)?;
        let decoded: UnifiedMessage = bincode::deserialize(&payload).unwrap();
        assert_eq!(decoded.tokens.input,10);
        assert_eq!(bincode::deserialize::<Option<String>>(&workspace).unwrap(),first.workspace_key);
        assert_eq!(bincode::deserialize::<Option<String>>(&label).unwrap(),first.workspace_label);
        Ok(())
    }).unwrap();
    assert!(cache
        .visit_version(
            bad.source_id,
            bad.generation,
            &mut |_: &UnifiedMessage, _, _| Ok(())
        )
        .is_err());
}

#[test]
fn parser_context_and_namespace_are_exact_source_keys() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let a = CacheIdentity::for_client(ClientId::Claude).with_scope(Path::new("home-a"));
    let b = CacheIdentity::for_client(ClientId::Claude).with_scope(Path::new("home-b"));
    let first = publish(&cache, a, &path, &[message("a:req", 10)]);
    assert!(cache.metadata(b, &path).unwrap().is_none());
    assert!(cache.get(b, &path).is_none());
    let second = publish(&cache, b, &path, &[message("b:req", 20)]);
    assert_ne!(first.source_id, second.source_id);
    assert_eq!(rows(&cache, &first)[0].0.tokens.input, 10);
    assert_eq!(rows(&cache, &second)[0].0.tokens.input, 20);
    let codex = CacheIdentity::for_client(ClientId::Codex);
    assert!(cache.metadata(codex, &path).unwrap().is_none());
}

#[test]
fn failed_source_producer_or_scope_registration_keeps_last_good_version() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let good = publish(&cache, identity, &path, &[message("old:req", 100)]);
    let failed = cache.write_source(
        identity,
        &path,
        good.fingerprint.clone(),
        None,
        None,
        None,
        &mut |_| {},
        &mut |_, _| Ok(()),
        |writer| {
            writer.push(&message("new:req", 200), false, false)?;
            Err("incomplete source read".to_string())
        },
    );
    assert!(failed.is_err());
    let failed = cache.write_source(
        identity,
        &path,
        good.fingerprint.clone(),
        None,
        None,
        None,
        &mut |_| {},
        &mut |_, _| Err("scope registration failed".to_string()),
        |writer| writer.push(&message("new:req", 200), false, false),
    );
    assert!(failed.is_err());
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        good.generation
    );
    assert_eq!(rows(&cache, &good)[0].0.tokens.input, 100);
    let failed = cache.write_source(
        identity,
        &path,
        good.fingerprint.clone(),
        None,
        None,
        None,
        &mut |_| {},
        &mut |_, _| Ok(()),
        |writer| {
            writer.push(&message("new:req", 200), false, false)?;
            fs::write(&path, b"changed during read\n").map_err(|error| error.to_string())?;
            Ok(())
        },
    );
    assert!(failed.is_err());
    assert_eq!(rows(&cache, &good)[0].0.tokens.input, 100);
}

#[test]
fn sqlite_full_rolls_back_without_erasing_unique_retained_history() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let mut cache = SourceMessageCache::open_from_root(Some(root.path()));
    cache.insert(
        CachedSourceEntry::new(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            vec![message("retained:req", 777)],
            vec![],
            None,
        )
        .with_retained_keys(HashSet::from(["retained:req".to_string()])),
    );
    cache.check().unwrap();
    let good = cache.metadata(identity, &path).unwrap().unwrap();
    let observed = cache.retention_metadata(identity, &path).unwrap();
    cache
        .with_database(None, |db| {
            let pages: i64 = db
                .query_row("PRAGMA page_count", [], |row| row.get(0))
                .map_err(sql_error)?;
            db.pragma_update(None, "max_page_count", pages)
                .map_err(sql_error)
        })
        .unwrap();
    let mut oversized = message("new:req", 1);
    oversized.model_id = "x".repeat(2 * 1024 * 1024);
    assert!(cache
        .write_source(
            identity,
            &path,
            good.fingerprint.clone(),
            observed.as_ref(),
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| writer.push(&oversized, false, false)
        )
        .is_err());
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        good.generation
    );
    let kept = rows(&cache, &good);
    assert_eq!(kept[0].0.tokens.input, 777);
    assert!(kept[0].1);
    assert!(root
        .path()
        .join("source-message-cache.recovery.sqlite3")
        .is_file());
}

#[test]
fn referenced_versions_survive_atomic_scope_gc_then_expire_after_unpinning() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    cache.ensure_open().unwrap();
    cache.with_database(None,|db| db.execute_batch(
        "CREATE TABLE sq_scope_sources(scope_generation INTEGER,cache_source_id INTEGER,cache_generation INTEGER);
         CREATE INDEX sq_cache_version ON sq_scope_sources(cache_source_id,cache_generation);",
    ).map_err(sql_error)).unwrap();
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let old = cache
        .write_source(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |db, metadata| {
                db.execute(
                    "INSERT INTO sq_scope_sources VALUES(1,?1,?2)",
                    params![metadata.source_id, metadata.generation],
                )
                .map(|_| ())
                .map_err(sql_error)
            },
            |writer| writer.push(&message("old:req", 1), false, false),
        )
        .unwrap();
    publish(&cache, identity, &path, &[message("new:req", 2)]);
    cache.collect_unreferenced_versions().unwrap();
    assert_eq!(rows(&cache, &old)[0].0.tokens.input, 1);
    cache
        .with_database(None, |db| {
            let transaction = db.unchecked_transaction().map_err(sql_error)?;
            transaction
                .execute("DELETE FROM sq_scope_sources", [])
                .map_err(sql_error)?;
            collect_unreferenced_versions_on(&transaction)?;
            transaction.commit().map_err(sql_error)
        })
        .unwrap();
    // Durable GC keeps the newest generation per source identity even with no
    // live scope reference; only superseded intermediates are collected. The
    // old generation is superseded by the newer write, so it expires here.
    assert!(cache
        .visit_version(
            old.source_id,
            old.generation,
            &mut |_: &UnifiedMessage, _, _| Ok(())
        )
        .is_err());
    assert_eq!(
        cache.get(identity, &path).unwrap().messages[0].tokens.input,
        2
    );
}

#[test]
fn complete_empty_and_uncacheable_sources_have_distinct_admission() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "empty.jsonl");
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let empty = publish(&cache, identity, &path, &[]);
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        empty.generation
    );
    let good = publish(&cache, identity, &path, &[message("retained:req", 100)]);
    let transient = cache
        .write_source(
            identity,
            &path,
            good.fingerprint.clone(),
            None,
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.set_cacheable(false)?;
                writer.push(&message("partial:req", 5), false, false)
            },
        )
        .unwrap();
    assert!(cache.metadata(identity, &path).unwrap().is_none());
    assert_eq!(rows(&cache, &transient)[0].0.tokens.input, 5);
    cache.collect_unreferenced_versions().unwrap();
    let observed = cache.retention_metadata(identity, &path).unwrap().unwrap();
    let mut inputs: Vec<_> = rows(&cache, &observed)
        .iter()
        .map(|row| row.0.tokens.input)
        .collect();
    inputs.sort_unstable();
    assert_eq!(inputs, vec![5, 100]);
    let recovered = publish(&cache, identity, &path, &[message("new:req", 10)]);
    assert!(rows(&cache, &recovered)
        .iter()
        .any(|(message, retained, _)| *retained
            && message.dedup_key.as_deref() == Some("retained:req")));
}

#[test]
fn legacy_codex_resume_membership_migrates_as_streamed_disk_rows() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "codex.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Codex);
    let fingerprint = SourceFingerprint::from_path(&path).unwrap();
    let state = CodexParseState {
        current_model: Some("gpt-5".to_string()),
        forked_child_task_started_turn_ids: (0..2049)
            .map(|index| format!("turn-{index}"))
            .collect(),
        ..Default::default()
    };
    let resume = build_codex_incremental_cache(&path, fingerprint.size, state).unwrap();
    let entry = CachedSourceEntry::new(
        identity,
        &path,
        fingerprint,
        vec![message("codex:req", 50)],
        vec![0],
        Some(resume),
    );
    let shard = seed_legacy(root.path(), identity, &path, &[entry]);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let metadata = cache.metadata(identity, &path).unwrap().unwrap();
    assert!(!shard.exists());
    assert!(metadata
        .codex_incremental
        .as_ref()
        .unwrap()
        .state
        .forked_child_task_started_turn_ids
        .is_empty());
    let mut count = 0;
    cache
        .visit_codex_turn_ids(metadata.source_id, metadata.generation, &mut |_: &str| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 2049);
    let materialized = cache.get(identity, &path).unwrap();
    assert_eq!(
        materialized
            .codex_incremental
            .unwrap()
            .state
            .forked_child_task_started_turn_ids
            .len(),
        2049
    );
    assert_eq!(materialized.fallback_timestamp_indices, vec![0]);
}

#[test]
fn codex_append_preserves_snapshot_fallback_and_clears_reset_membership() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "codex.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Codex);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let fingerprint = SourceFingerprint::from_path(&path).unwrap();
    let resume =
        build_codex_incremental_cache(&path, fingerprint.size, CodexParseState::default()).unwrap();
    let initial = cache
        .write_source(
            identity,
            &path,
            fingerprint,
            None,
            None,
            Some(resume),
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.insert_codex_turn_id("old-turn")?;
                writer.push(&message("old:req", 1), false, true)
            },
        )
        .unwrap();
    fs::write(&path, b"source\nappended\n").unwrap();
    let fingerprint = SourceFingerprint::from_path(&path).unwrap();
    assert!(codex_prefix_matches(
        &path,
        initial.codex_incremental.as_ref().unwrap()
    ));
    let expected_timestamp = (fingerprint.modified_ns / 1_000_000) as i64;
    let resume =
        build_codex_incremental_cache(&path, fingerprint.size, CodexParseState::default()).unwrap();
    let appended = cache
        .write_source(
            identity,
            &path,
            fingerprint,
            None,
            Some(&initial),
            Some(resume),
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.clear_codex_turn_ids()?;
                writer.insert_codex_turn_id("new-turn")?;
                writer.push(&message("new:req", 2), false, true)
            },
        )
        .unwrap();
    let mut ids = Vec::new();
    cache
        .visit_codex_turn_ids(appended.source_id, appended.generation, &mut |id: &str| {
            ids.push(id.to_string());
            Ok(())
        })
        .unwrap();
    assert_eq!(ids, vec!["new-turn"]);
    let mut emitted = Vec::new();
    cache
        .visit_version_range(
            appended.source_id,
            appended.generation,
            Some(expected_timestamp),
            expected_timestamp.checked_add(1),
            &mut |message: &UnifiedMessage, _, fallback| {
                emitted.push((message.timestamp, fallback));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(emitted, vec![(expected_timestamp, true); 2]);
    assert_eq!(
        rows(&cache, &initial).len(),
        1,
        "the pinned prefix snapshot remains immutable"
    );
}

#[test]
fn malformed_legacy_transfer_preserves_shard_and_prior_published_rows() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let good = publish(&cache, identity, &path, &[message("old:req", 100)]);
    let entry = CachedSourceEntry::new(
        identity,
        &path,
        good.fingerprint.clone(),
        vec![message("partial:req", 200)],
        vec![],
        None,
    );
    let shard = seed_legacy(root.path(), identity, &path, &[entry]);
    let mut envelope: CachedShardEnvelope = decode(&fs::read(&shard).unwrap()).unwrap();
    envelope.payload.pop();
    fs::write(&shard, encode(&envelope).unwrap()).unwrap();
    let bytes = fs::read(&shard).unwrap();
    assert!(cache.metadata(identity, &path).is_err());
    assert_eq!(fs::read(&shard).unwrap(), bytes);
    assert_eq!(rows(&cache, &good)[0].0.tokens.input, 100);
}

#[test]
fn later_legacy_writer_is_detected_and_scoped_history_is_not_lost() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let unscoped = CacheIdentity::for_client(ClientId::Claude);
    let scoped = unscoped.with_scope(sources.path());
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(&cache, scoped, &path, &[message("known:req", 100)]);
    let legacy = CachedSourceEntry::new(
        unscoped,
        &path,
        SourceFingerprint::from_path(&path).unwrap(),
        vec![message("late:req", 999)],
        vec![],
        None,
    );
    let shard = seed_legacy(root.path(), unscoped, &path, &[legacy]);
    assert!(
        cache.metadata(scoped, &path).unwrap().is_none(),
        "an unchanged raw fingerprint cannot hide new legacy history"
    );
    assert!(!shard.exists());
    let observed = cache.retention_metadata(scoped, &path).unwrap().unwrap();
    assert!(observed.legacy_history.is_some());
    let query = publish(&cache, scoped, &path, &[]);
    let messages = rows(&cache, &query);
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|(_, retained, _)| *retained));
    assert!(cache.metadata(scoped, &path).unwrap().is_some());
}

#[test]
fn corruption_requires_explicit_offline_recovery_and_preserves_last_backup() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let mut cache = SourceMessageCache::open_from_root(Some(root.path()));
    cache.insert(
        CachedSourceEntry::new(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            vec![message("unique:req", 777)],
            vec![],
            None,
        )
        .with_retained_keys(HashSet::from(["unique:req".to_string()])),
    );
    cache.check().unwrap();
    drop(cache);
    let database = root.path().join("source-message-cache.sqlite3");
    let recovery = root.path().join("source-message-cache.recovery.sqlite3");
    let backup = fs::read(&recovery).unwrap();
    fs::write(&database, b"invalid canonical database").unwrap();
    let broken = SourceMessageCache::open_from_root(Some(root.path()));
    assert!(broken.metadata(identity, &path).is_err());
    assert_eq!(fs::read(&database).unwrap(), b"invalid canonical database");
    assert_eq!(fs::read(&recovery).unwrap(), backup);
    assert!(
        recover_source_message_cache(root.path()).is_err(),
        "recovery never overwrites a live/invalid canonical store"
    );
    drop(broken);
    fs::rename(&database, root.path().join("preserved-invalid.sqlite3")).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = root
            .path()
            .join(format!("source-message-cache.sqlite3{suffix}"));
        if sidecar.exists() {
            fs::rename(
                &sidecar,
                root.path()
                    .join(format!("preserved-invalid.sqlite3{suffix}")),
            )
            .unwrap();
        }
    }
    recover_source_message_cache(root.path()).unwrap();
    let restored = SourceMessageCache::open_from_root(Some(root.path()));
    let entry = restored.get(identity, &path).unwrap();
    assert_eq!(entry.messages[0].tokens.input, 777);
    assert!(entry.retained_keys.contains("unique:req"));
}

#[test]
fn unsupported_schema_and_missing_rows_are_errors_not_empty_success() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let good = publish(
        &cache,
        identity,
        &path,
        &[message("one:req", 1), message("two:req", 2)],
    );
    cache
        .with_database(None, |db| {
            db.execute(
                "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2 AND ordinal=1",
                params![good.source_id, good.generation],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    assert!(cache
        .visit_version_range(
            good.source_id,
            good.generation,
            Some(0),
            Some(200),
            &mut |_: &UnifiedMessage, _, _| Ok(())
        )
        .is_err());
    cache
        .with_database(None, |db| {
            db.execute("UPDATE mc_meta SET value=99 WHERE key='schema_version'", [])
                .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    assert!(
        cache.metadata(identity, &path).is_err(),
        "an already-open connection must respect schema admission changes"
    );
    let reopened = SourceMessageCache::open_from_root(Some(root.path()));
    assert!(reopened.ensure_open().is_err());
    assert!(root.path().join("source-message-cache.sqlite3").is_file());
}

#[test]
fn stale_invalidation_does_not_delete_a_later_writer_generation() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let first = SourceMessageCache::open_from_root(Some(root.path()));
    publish(&first, identity, &path, &[message("old:req", 1)]);
    let mut stale = SourceMessageCache::load_from_root(Some(root.path()));
    let fresh = SourceMessageCache::open_from_root(Some(root.path()));
    fs::write(&path, b"fresh source\n").unwrap();
    let new = publish(&fresh, identity, &path, &[message("new:req", 999)]);
    stale.remove(identity, &path);
    stale.check().unwrap();
    assert_eq!(
        fresh.metadata(identity, &path).unwrap().unwrap().generation,
        new.generation
    );
    assert_eq!(rows(&fresh, &new)[0].0.tokens.input, 999);
}

#[test]
fn scope_reconciliation_preserves_absent_sources_as_durable_history() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let gone = source(sources.path(), "gone.jsonl");
    let filtered = source(sources.path(), "filtered.jsonl");
    let other = source(sources.path(), "other.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let gone_version = publish(&cache, identity, &gone, &[message("gone:req", 1)]);
    let filtered_version = publish(&cache, identity, &filtered, &[message("filtered:req", 2)]);
    let other_version = publish(&cache, identity, &other, &[message("other:req", 3)]);
    cache.with_database(None,|db| {
        db.execute_batch(
            "CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,scope_id INTEGER,complete INTEGER);
             INSERT INTO sq_scope_generations VALUES(1,1,1),(2,1,0),(3,2,1),(4,2,0);
             CREATE TABLE sq_scope_sources(scope_generation INTEGER,kind TEXT,cache_source_id INTEGER,cache_generation INTEGER);
             CREATE INDEX sq_cache_version ON sq_scope_sources(cache_source_id,cache_generation);",
        ).map_err(sql_error)?;
        for (scope,metadata) in [(1,&gone_version),(1,&filtered_version),(3,&other_version)] {
            db.execute("INSERT INTO sq_scope_sources VALUES(?1,'cache',?2,?3)",
                params![scope,metadata.source_id,metadata.generation]).map_err(sql_error)?;
        }
        Ok(())
    }).unwrap();
    fs::remove_file(&gone).unwrap();
    fs::remove_file(&other).unwrap();
    cache
        .with_database(None, |db| {
            let transaction = db.unchecked_transaction().map_err(sql_error)?;
            assert!(
                reconcile_missing_sources_on(&transaction, 1, 4).is_err(),
                "cross-account/root scope retirement is rejected"
            );
            reconcile_missing_sources_on(&transaction, 1, 2)?;
            transaction.commit().map_err(sql_error)
        })
        .unwrap();
    // Absent sources stay queryable as durable history, not unlinked.
    assert_eq!(
        cache.metadata(identity, &gone).unwrap().unwrap().generation,
        gone_version.generation
    );
    assert_eq!(
        cache
            .metadata(identity, &filtered)
            .unwrap()
            .unwrap()
            .generation,
        filtered_version.generation
    );
    assert_eq!(
        cache
            .metadata(identity, &other)
            .unwrap()
            .unwrap()
            .generation,
        other_version.generation
    );
    assert_eq!(
        rows(&cache, &gone_version)[0].0.tokens.input,
        1,
        "older complete scope/reader versions remain available"
    );
}

#[test]
fn codex_membership_corruption_aborts_resume_seed() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "codex.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Codex);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let fingerprint = SourceFingerprint::from_path(&path).unwrap();
    let resume =
        build_codex_incremental_cache(&path, fingerprint.size, CodexParseState::default()).unwrap();
    let version = cache
        .write_source(
            identity,
            &path,
            fingerprint,
            None,
            None,
            Some(resume),
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.insert_codex_turn_id("one")?;
                writer.insert_codex_turn_id("two")?;
                writer.push(&message("codex:req", 1), false, false)
            },
        )
        .unwrap();
    cache.with_database(None,|db| {
        db.execute("DELETE FROM mc_codex_turn_ids WHERE source_id=?1 AND generation=?2 AND turn_id='two'",
            params![version.source_id,version.generation]).map_err(sql_error)?;
        Ok(())
    }).unwrap();
    assert!(cache
        .visit_codex_turn_ids(version.source_id, version.generation, &mut |_: &str| Ok(()))
        .is_err());
}

#[test]
fn oversized_single_compact_row_is_preserved_instead_of_dropped() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "large.jsonl");
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let mut large = message("large:req", 1);
    large.model_id = "m".repeat(8 * 1024 * 1024 + 1);
    let version = publish(&cache, identity, &path, std::slice::from_ref(&large));
    let mut seen = 0;
    cache
        .visit_version(
            version.source_id,
            version.generation,
            &mut |message: &UnifiedMessage, _, _| {
                assert_eq!(message.model_id.len(), large.model_id.len());
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen, 1);
}

#[test]
fn failed_retained_recovery_does_not_retire_a_transferred_legacy_shard() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let entry = CachedSourceEntry::new(
        identity,
        &path,
        SourceFingerprint::from_path(&path).unwrap(),
        vec![message("only-copy:req", 777)],
        vec![],
        None,
    )
    .with_retained_keys(HashSet::from(["only-copy:req".to_string()]));
    let shard = seed_legacy(root.path(), identity, &path, &[entry]);
    let original = fs::read(&shard).unwrap();
    let recovery = root.path().join("source-message-cache.recovery.sqlite3");
    fs::create_dir(&recovery).unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    assert!(cache.metadata(identity, &path).is_err());
    assert_eq!(fs::read(&shard).unwrap(), original);
    fs::remove_dir(&recovery).unwrap();
    let transferred = cache.metadata(identity, &path).unwrap().unwrap();
    assert_eq!(rows(&cache, &transferred)[0].0.tokens.input, 777);
    assert!(
        !shard.exists(),
        "retirement resumes only after verified retained recovery succeeds"
    );
}

#[test]
fn parser_upgrade_carries_missing_history_without_max_merging_reparsed_live_keys() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let old = publish(
        &cache,
        identity,
        &path,
        &[message("shared:req", 100), message("dropped:req", 200)],
    );
    cache
        .with_database(None, |db| {
            db.execute(
                "UPDATE mc_versions SET parser_version=?1 WHERE source_id=?2",
                params![identity.parser_version - 1, old.source_id],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    assert!(cache.metadata(identity, &path).unwrap().is_none());
    let observed = cache.retention_metadata(identity, &path).unwrap();
    let query = cache
        .write_source(
            identity,
            &path,
            old.fingerprint,
            observed.as_ref(),
            None,
            None,
            &mut |message| {
                crate::sessions::claudecode::refresh_retained_message_context(
                    message, &path, None, None,
                )
            },
            &mut |_, _| Ok(()),
            |writer| writer.push(&message("shared:req", 50), false, false),
        )
        .unwrap();
    assert_eq!(rows(&cache, &query)[0].0.tokens.input, 50);
    let warm = cache.metadata(identity, &path).unwrap().unwrap();
    let retained = rows(&cache, &warm);
    assert_eq!(
        retained[0].0.tokens.input, 50,
        "older parser semantics are not reconciled into a new live parse"
    );
    assert!(retained
        .iter()
        .any(|(message, retained, _)| *retained
            && message.dedup_key.as_deref() == Some("dropped:req")));
}

#[test]
fn corrupted_observed_and_save_generations_never_normalize_retained_history() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(
        &cache,
        identity,
        &path,
        &[message("shared:req", 100), message("dropped:req", 777)],
    );
    fs::write(&path, b"compacted\n").unwrap();
    publish(&cache, identity, &path, &[message("shared:req", 50)]);
    let observed = cache.retention_metadata(identity, &path).unwrap().unwrap();
    publish(&cache, identity, &path, &[message("shared:req", 50)]);
    let latest = cache.metadata(identity, &path).unwrap().unwrap();
    let backup_path = root.path().join("source-message-cache.recovery.sqlite3");
    let backup = fs::read(&backup_path).unwrap();
    cache
        .with_database(None, |db| {
            db.execute(
                "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2 AND retained=1",
                params![observed.source_id, observed.generation],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    let attempt = |history: Option<&CachedSourceMetadata>| {
        cache.write_source(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            history,
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| writer.push(&message("shared:req", 10), false, false),
        )
    };
    assert!(
        attempt(Some(&observed)).is_err(),
        "incomplete earlier observed version must abort even when latest is intact"
    );
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        latest.generation
    );
    assert_eq!(fs::read(&backup_path).unwrap(), backup);
    cache
        .with_database(None, |db| {
            db.execute(
                "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2 AND retained=1",
                params![latest.source_id, latest.generation],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    assert!(
        attempt(None).is_err(),
        "latest save-time history is validated independently of observed history"
    );
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        latest.generation
    );
    assert_eq!(fs::read(&backup_path).unwrap(), backup);
}

#[test]
fn codex_append_rejects_missing_or_corrupt_prefix_rows() {
    for corrupt_payload in [false, true] {
        let sources = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let path = source(sources.path(), "codex.jsonl");
        let identity = CacheIdentity::for_client(ClientId::Codex);
        let cache = SourceMessageCache::open_from_root(Some(root.path()));
        let fingerprint = SourceFingerprint::from_path(&path).unwrap();
        let resume =
            build_codex_incremental_cache(&path, fingerprint.size, CodexParseState::default())
                .unwrap();
        let initial = cache
            .write_source(
                identity,
                &path,
                fingerprint,
                None,
                None,
                Some(resume),
                &mut |_| {},
                &mut |_, _| Ok(()),
                |writer| {
                    writer.push(&message("one:req", 1), false, false)?;
                    writer.push(&message("two:req", 2), false, false)
                },
            )
            .unwrap();
        cache.with_database(None,|db| {
            let sql = if corrupt_payload {
                "UPDATE mc_rows SET message=x'ff' WHERE source_id=?1 AND generation=?2 AND ordinal=1"
            } else {
                "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2 AND ordinal=1"
            };
            db.execute(sql,params![initial.source_id,initial.generation]).map_err(sql_error)?;Ok(())
        }).unwrap();
        fs::write(&path, b"source\nappended\n").unwrap();
        let fingerprint = SourceFingerprint::from_path(&path).unwrap();
        let resume =
            build_codex_incremental_cache(&path, fingerprint.size, CodexParseState::default())
                .unwrap();
        assert!(cache
            .write_source(
                identity,
                &path,
                fingerprint,
                None,
                Some(&initial),
                Some(resume),
                &mut |_| {},
                &mut |_, _| Ok(()),
                |writer| writer.push(&message("new:req", 3), false, false)
            )
            .is_err());
        assert_eq!(
            cache.metadata(identity, &path).unwrap().unwrap().generation,
            initial.generation
        );
    }
}

#[test]
fn resumed_migration_revalidates_transferred_rows_before_backup_or_unlink() {
    for corrupt_payload in [false, true] {
        let sources = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let kept_path = source(sources.path(), "kept.jsonl");
        let imported_path = source(sources.path(), "imported.jsonl");
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let mut cache = SourceMessageCache::open_from_root(Some(root.path()));
        cache.insert(
            CachedSourceEntry::new(
                identity,
                &kept_path,
                SourceFingerprint::from_path(&kept_path).unwrap(),
                vec![message("kept:req", 42)],
                vec![],
                None,
            )
            .with_retained_keys(HashSet::from(["kept:req".to_string()])),
        );
        cache.check().unwrap();
        let old_scope = cache.metadata(identity, &kept_path).unwrap().unwrap();
        let recovery = root.path().join("source-message-cache.recovery.sqlite3");
        let preserved = root.path().join("last-good-recovery.sqlite3");
        let old_backup = fs::read(&recovery).unwrap();
        fs::rename(&recovery, &preserved).unwrap();
        fs::create_dir(&recovery).unwrap();
        let entry = CachedSourceEntry::new(
            identity,
            &imported_path,
            SourceFingerprint::from_path(&imported_path).unwrap(),
            vec![message("imported:req", 777)],
            vec![],
            None,
        )
        .with_retained_keys(HashSet::from(["imported:req".to_string()]));
        let shard = seed_legacy(root.path(), identity, &imported_path, &[entry]);
        let legacy_bytes = fs::read(&shard).unwrap();
        assert!(
            cache.metadata(identity, &imported_path).is_err(),
            "backup failure keeps the intact shard after committed transfer"
        );
        let imported = cache
            .with_database(None, |db| {
                Ok(read_metadata(db, identity, &imported_path)?
                    .unwrap()
                    .metadata)
            })
            .unwrap();
        cache.collect_unreferenced_versions().unwrap();
        cache
            .with_database(None, |db| {
                let sql = if corrupt_payload {
                    "UPDATE mc_rows SET message=x'ff' WHERE source_id=?1 AND generation=?2"
                } else {
                    "DELETE FROM mc_rows WHERE source_id=?1 AND generation=?2"
                };
                db.execute(sql, params![imported.source_id, imported.generation])
                    .map_err(sql_error)?;
                Ok(())
            })
            .unwrap();
        fs::remove_dir(&recovery).unwrap();
        fs::rename(&preserved, &recovery).unwrap();
        assert!(
            cache.metadata(identity, &imported_path).is_err(),
            "a marker cannot prove that transferred rows still exist"
        );
        assert_eq!(fs::read(&shard).unwrap(), legacy_bytes);
        assert_eq!(fs::read(&recovery).unwrap(), old_backup);
        assert_eq!(rows(&cache, &old_scope)[0].0.tokens.input, 42);
        assert_eq!(cache.with_database(None,|db| db.query_row(
            "SELECT count(*) FROM mc_migration_versions WHERE source_id=?1 AND generation=?2",
            params![imported.source_id,imported.generation],|row| row.get::<_,u64>(0)).map_err(sql_error)).unwrap(),1);
    }
}

/// The `claude_cache_units` rewrite in miniature: a bare whole-prompt snapshot
/// (input 42494) and a split uncached remainder (input 265 + cache 42000) race
/// through the same save-time union. One policy must hold in either merge
/// order, with the comparable buckets still taking their maxima.
#[test]
fn claude_rewrite_merge_prefers_split_input_in_either_order() {
    for split_saves_first in [false, true] {
        let sources = TempDir::new().unwrap();
        let root = TempDir::new().unwrap();
        let path = source(sources.path(), "session.jsonl");
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let cache = SourceMessageCache::open_from_root(Some(root.path()));
        let mut bare = split_message("same-message:same-request", 42494, false, 0);
        bare.tokens.output = 200;
        let split = split_message("same-message:same-request", 265, true, 42000);
        let (first, second) = if split_saves_first {
            (split, bare)
        } else {
            (bare, split)
        };
        // The first writer's generation is the observed concurrent history the
        // second writer unions against at save time.
        let first_meta = publish(&cache, identity, &path, std::slice::from_ref(&first));
        let observed = cache
            .retention_metadata(identity, &path)
            .unwrap()
            .filter(|meta| {
                meta.source_id == first_meta.source_id && meta.generation == first_meta.generation
            });
        fs::write(&path, b"rewritten source observation\n").unwrap();
        let merged = cache
            .write_source(
                identity,
                &path,
                SourceFingerprint::from_path(&path).unwrap(),
                observed.as_ref(),
                None,
                None,
                &mut |message| {
                    crate::sessions::claudecode::refresh_retained_message_context(
                        message, &path, None, None,
                    )
                },
                &mut |_, _| Ok(()),
                |writer| writer.push(&second, false, false),
            )
            .unwrap();
        // The shared key must reconcile to the split uncached remainder, not
        // the bare whole-prompt snapshot.
        let merged = rows(&cache, &merged);
        assert_eq!(merged.len(), 1, "split_saves_first={split_saves_first}");
        assert_eq!(
            (merged[0].0.tokens.input, merged[0].0.tokens.cache_read),
            (265, 42000),
            "split_saves_first={split_saves_first}"
        );
        assert!(
            merged[0].0.input_tokens_are_cache_split,
            "split_saves_first={split_saves_first}"
        );
        // Comparable buckets still take their maxima across the two writers.
        assert_eq!(merged[0].0.tokens.output, 200);
    }
}

fn frozen_format4(message: UnifiedMessage) -> format4::UnifiedMessage {
    format4::UnifiedMessage {
        client: message.client,
        model_id: message.model_id,
        provider_id: message.provider_id,
        session_id: message.session_id,
        workspace_key: message.workspace_key,
        workspace_label: message.workspace_label,
        timestamp: message.timestamp,
        date: message.date,
        tokens: format4::TokenBreakdown {
            input: message.tokens.input,
            output: message.tokens.output,
            cache_read: message.tokens.cache_read,
            cache_write: message.tokens.cache_write,
            reasoning: message.tokens.reasoning,
            cache_write_1h: message.tokens.cache_write_1h,
        },
        cost: message.cost,
        cost_source: message.cost_source,
        duration_ms: message.duration_ms,
        message_count: message.message_count,
        agent: message.agent,
        dedup_key: message.dedup_key,
        dedup_aliases: message.dedup_aliases,
        is_turn_start: message.is_turn_start,
    }
}

fn seed_format4_rows(
    cache: &SourceMessageCache,
    metadata: &CachedSourceMetadata,
    message: UnifiedMessage,
) {
    let bytes = encode_fact(&frozen_format4(message)).unwrap();
    cache.with_database(None, |db| {
        db.execute(
            "UPDATE mc_rows SET message=?1,digest=?2 WHERE source_id=?3 AND generation=?4",
            params![bytes,Sha256::digest(&bytes).as_slice(),metadata.source_id,metadata.generation],
        ).map_err(sql_error)?;
        db.execute(
            "UPDATE mc_versions SET format_version=4,parser_version=4 WHERE source_id=?1 AND generation=?2",
            params![metadata.source_id,metadata.generation],
        ).map_err(sql_error)?;
        Ok(())
    }).unwrap();
}

#[test]
fn non_retaining_format4_is_a_cold_miss_not_an_unavailable_cache() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "amp.json");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let mut live = message("amp:req", 9);
    live.client = identity.namespace.to_string();
    let old = publish(&cache, identity, &path, std::slice::from_ref(&live));
    seed_format4_rows(&cache, &old, live.clone());
    cache
        .with_database(None, |db| {
            db.execute(
                "UPDATE mc_versions SET parser_version=?1 WHERE source_id=?2 AND generation=?3",
                params![identity.parser_version, old.source_id, old.generation],
            )
            .map_err(sql_error)?;
            Ok(())
        })
        .unwrap();
    assert!(cache.metadata(identity, &path).unwrap().is_none());
    let current = publish(&cache, identity, &path, &[live]);
    assert_eq!(
        cache.metadata(identity, &path).unwrap().unwrap().generation,
        current.generation
    );
    let historical = rows(&cache, &old);
    assert_eq!(historical[0].0.tokens.input, 9);
    assert!(!historical[0].0.input_tokens_are_cache_split);
}

#[test]
fn format4_retained_rows_survive_publication_and_reopen_without_inventing_provenance() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let mut retained = message("retained:req", 265);
    retained.tokens.cache_read = 42000;
    let mut metadata = publish(&cache, identity, &path, std::slice::from_ref(&retained));
    seed_format4_rows(&cache, &metadata, retained);
    for migrate in [false, true] {
        if migrate {
            metadata = publish(&cache, identity, &path, &[]);
        }
        let found = rows(&cache, &metadata);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].0.tokens.input, found[0].0.tokens.cache_read),
            (265, 42000)
        );
        assert!(!found[0].0.input_tokens_are_cache_split);
    }
    drop(cache);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let found = cache.get(identity, &path).unwrap();
    assert_eq!(found.messages[0].tokens.input, 265);
}

/// The legacy-bin visitor must accept a real format-4 shard: its messages
/// decode through the same frozen shape as recorded-row format 4, land with
/// unknown provenance, and survive as retained-only history — never dropped.
#[test]
fn legacy_format4_shard_migrates_with_unknown_provenance() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let fingerprint = SourceFingerprint::from_path(&path).unwrap();
    let existing_cache = SourceMessageCache::open_from_root(Some(root.path()));
    let existing = message("existing:req", 9);
    let metadata = publish(
        &existing_cache,
        identity,
        &path,
        std::slice::from_ref(&existing),
    );
    seed_format4_rows(&existing_cache, &metadata, existing);
    drop(existing_cache);
    let mut retained = message("retained:req", 265);
    retained.tokens.cache_read = 42000;
    let frozen = frozen_format4(retained);
    // format4::CachedSourceEntry is test-gated like the rest of the legacy
    // path; this seed lives in the same test build.
    let entry = format4::CachedSourceEntry {
        parser_namespace: identity.namespace.to_string(),
        parser_version: 4,
        path: CachedPath::from_path(&path),
        fingerprint: fingerprint.clone(),
        messages: vec![frozen],
        fallback_timestamp_indices: vec![],
        codex_incremental: None,
        retained_keys: HashSet::from(["retained:req".to_string()]),
    };
    let shard = root
        .path()
        .join(CACHE_SHARD_DIRNAME)
        .join(identity.namespace)
        .join(format!(
            "shard-{:02x}.bin",
            legacy_shard_index(identity.namespace, &CachedPath::from_path(&path))
        ));
    fs::create_dir_all(shard.parent().unwrap()).unwrap();
    let envelope = CachedShardEnvelope {
        format_version: 4,
        parser_namespace: identity.namespace.to_string(),
        parser_version: 4,
        // Same varint codec the migration reader uses (`encode`, not the
        // fixed-int row codec): envelope and payload share the legacy layout.
        payload: encode(&vec![entry]).unwrap(),
    };
    fs::write(&shard, encode(&envelope).unwrap()).unwrap();
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    // Trigger migration through a fresh write, then read the retained history.
    publish(&cache, identity, &path, &[]);
    assert!(
        !shard.exists(),
        "a migrated format-4 shard retires like format-3"
    );
    let found = cache
        .retainable_history(identity, &path)
        .into_iter()
        .find(|message| message.dedup_key.as_deref() == Some("retained:req"))
        .expect("format-4 retained-only history must survive migration");
    assert_eq!((found.tokens.input, found.tokens.cache_read), (265, 42000));
    assert!(
        !found.input_tokens_are_cache_split,
        "migrated format-4 history never recorded provenance"
    );
    assert!(cache
        .retainable_history(identity, &path)
        .iter()
        .any(
            |message| message.dedup_key.as_deref() == Some("existing:req")
                && message.tokens.input == 9
        ));
}

#[test]
fn durable_facts_survive_source_deletion_across_reopen() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let published = publish(
        &cache,
        identity,
        &path,
        &[message("a:req", 10), message("b:req", 50)],
    );
    assert_eq!(
        rows(&cache, &published)
            .iter()
            .map(|row| row.0.tokens.input)
            .collect::<Vec<_>>(),
        vec![10, 50]
    );
    drop(cache);
    // Whole-file removal must not erase canonical facts: a cold reopen
    // serves the same generation rows without the native source present.
    std::fs::remove_file(&path).unwrap();
    let reopened = SourceMessageCache::open_from_root(Some(root.path()));
    let metadata = reopened.metadata(identity, &path).unwrap().unwrap();
    assert_eq!(metadata.generation, published.generation);
    let mut values = Vec::new();
    reopened
        .visit_version(
            metadata.source_id,
            metadata.generation,
            &mut |message: &UnifiedMessage, _, _| {
                values.push(message.tokens.input);
                Ok(())
            },
        )
        .unwrap();
    values.sort();
    assert_eq!(values, vec![10, 50]);
}

#[test]
fn durable_truncate_reappend_reconciles_same_identity_without_double_count() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(&cache, identity, &path, &[message("a:req", 10)]);
    // Truncate then re-append the same stable identity with updated values:
    // exactly one row, latest observation wins, no duplicate.
    std::fs::write(&path, b"truncated\n").unwrap();
    let updated = publish(&cache, identity, &path, &[message("a:req", 12)]);
    let found = rows(&cache, &updated);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.tokens.input, 12);
}

#[test]
fn durable_duplicated_stable_ids_keep_both_identities_without_merging() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    // Same stable key copied to a second file: storage keeps both source
    // identities intact (no cross-source merge); selection arbitrates.
    for name in ["original.jsonl", "copied.jsonl"] {
        let path = source(sources.path(), name);
        let published = publish(&cache, identity, &path, &[message("shared:req", 10)]);
        assert_eq!(rows(&cache, &published).len(), 1);
    }
}

#[test]
fn durable_parser_upgrade_keeps_retained_only_history_decodable() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let old = publish(
        &cache,
        identity,
        &path,
        &[message("shared:req", 100), message("dropped:req", 200)],
    );
    cache
        .with_database(None, |db| {
            db.execute(
                "UPDATE mc_versions SET parser_version=?1 WHERE source_id=?2",
                params![identity.parser_version - 1, old.source_id],
            )
            .map(|_| ())
            .map_err(sql_error)
        })
        .unwrap();
    // Source missing at upgrade time: retained-only rows must still decode
    // through the frozen format-4/5 readers rather than erroring.
    std::fs::remove_file(&path).unwrap();
    assert!(cache.metadata(identity, &path).unwrap().is_none());
    let durable = cache.retention_metadata(identity, &path).unwrap().unwrap();
    let mut values = Vec::new();
    cache
        .visit_version(
            durable.source_id,
            durable.generation,
            &mut |message: &UnifiedMessage, _, _| {
                values.push(message.tokens.input);
                Ok(())
            },
        )
        .unwrap();
    values.sort();
    assert_eq!(values, vec![100, 200]);
}

/// Production-path durable regression: a non-Claude lane written through the
/// real `write_source` merge path retains rows the live file no longer
/// contains, keyed by stable dedup key, without duplicating survivors.
#[test]
fn durable_non_claude_truncate_keeps_missing_stable_rows_through_stage() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "amp.jsonl");
    let identity = CacheIdentity::for_client(crate::ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(
        &cache,
        identity,
        &path,
        &[message("keep:req", 10), message("drop:req", 50)],
    );
    // Truncate the live file to one row; the missing stable row must carry
    // through the production write/merge path, not just row pinning.
    std::fs::write(&path, b"v2\n").unwrap();
    publish(&cache, identity, &path, &[message("keep:req", 10)]);
    let metadata = cache.metadata(identity, &path).unwrap().unwrap();
    let found = rows(&cache, &metadata);
    assert_eq!(
        found.len(),
        2,
        "truncated-away stable row must carry forward"
    );
    let mut inputs: Vec<i64> = found.iter().map(|row| row.0.tokens.input).collect();
    inputs.sort();
    assert_eq!(inputs, vec![10, 50]);
}

/// First changed-source call through the production write path: a non-Claude
/// changed fingerprint reconciles (fresh rows win same-key, missing stable
/// rows carry) on the very first write — no warm-up call required.
#[test]
fn durable_non_claude_first_changed_source_reconciles_without_warmup() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "amp.jsonl");
    let identity = CacheIdentity::for_client(crate::ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let first = publish(
        &cache,
        identity,
        &path,
        &[message("keep:req", 10), message("drop:req", 50)],
    );
    assert_eq!(rows(&cache, &first).len(), 2);
    // Changed content, first call since the seed: keep updates, drop carries.
    std::fs::write(&path, b"changed\n").unwrap();
    let observed = cache.retention_metadata(identity, &path).unwrap();
    let merged = cache
        .write_source(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            observed.as_ref(),
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.push(&message("keep:req", 12), false, false)?;
                writer.push(&message("new:req", 7), false, false)?;
                Ok(())
            },
        )
        .unwrap();
    let found = rows(&cache, &merged);
    assert_eq!(found.len(), 3, "keep updates, drop carries, new adds");
    let mut inputs: Vec<i64> = found.iter().map(|row| row.0.tokens.input).collect();
    inputs.sort();
    assert_eq!(inputs, vec![7, 12, 50]);
}

/// Keyless event rows retain source-local multiplicity: an omitted event must
/// survive, but a fresh occurrence consumes exactly one historical occurrence.
#[test]
fn durable_keyless_rows_are_never_dropped_by_history_merge() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "amp.jsonl");
    let identity = CacheIdentity::for_client(crate::ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    let mut first_keyless = message("", 5);
    first_keyless.dedup_key = None;
    let mut second_keyless = message("", 5);
    second_keyless.dedup_key = None;
    let first = publish(
        &cache,
        identity,
        &path,
        &[message("keyed:req", 10), first_keyless, second_keyless],
    );
    assert_eq!(rows(&cache, &first).len(), 3);
    // Rewrite omits BOTH keyless facts and keeps only the keyed row: both
    // carried forward verbatim (multiplicity preserved, no collapse).
    std::fs::write(&path, b"changed\n").unwrap();
    let second = publish(&cache, identity, &path, &[message("keyed:req", 10)]);
    let found = rows(&cache, &second);
    assert_eq!(found.len(), 3, "omitted keyless facts must carry forward");
    let keyless: Vec<_> = found
        .iter()
        .filter(|row| row.0.dedup_key.is_none())
        .collect();
    assert_eq!(keyless.len(), 2, "equal-valued repeats must not collapse");
    let repeated = keyless[0].0.clone();
    std::fs::write(&path, b"one repeat restored\n").unwrap();
    let restored = publish(
        &cache,
        identity,
        &path,
        &[message("keyed:req", 10), repeated.clone()],
    );
    let restored_rows = rows(&cache, &restored);
    assert_eq!(
        restored_rows
            .iter()
            .map(|row| row.0.tokens.input)
            .sum::<i64>(),
        20
    );
    std::fs::write(&path, b"three repeats observed\n").unwrap();
    let expanded = publish(
        &cache,
        identity,
        &path,
        &[
            message("keyed:req", 10),
            repeated.clone(),
            repeated.clone(),
            repeated,
        ],
    );
    assert_eq!(
        rows(&cache, &expanded)
            .iter()
            .map(|row| row.0.tokens.input)
            .sum::<i64>(),
        25
    );
}

#[test]
fn upstream_format10_claude_migration_preserves_source_free_usage() {
    let sources = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let wire_message = |key: &str, input| upstream10::UnifiedMessage {
        client: "claude".into(),
        model_id: "claude-3-5-sonnet".into(),
        provider_id: "anthropic".into(),
        session_id: "session".into(),
        workspace_key: None,
        workspace_label: None,
        timestamp: 100,
        date: "1970-01-01".into(),
        tokens: upstream10::TokenBreakdown {
            input,
            output: 11,
            cache_read: 42,
            cache_write: 31,
            cache_write_1h: 17,
            reasoning: 3,
        },
        cost: 0.0,
        cost_source: crate::CostSource::Unknown,
        service_tier: None,
        duration_ms: Some(123),
        message_count: 2,
        agent: None,
        dedup_key: Some(key.into()),
        session_title: Some("session title".into()),
        parent_session_id: None,
        is_turn_start: true,
        model_attribution_conflicted: false,
    };
    // Upstream has nine entry fields, no retained-key set, and puts Claude
    // retained ordinals plus its marker in the fallback vector.
    let entry = (
        "claude".to_string(),
        2_u32,
        CachedPath::from_path(&path),
        SourceFingerprint::from_path(&path).unwrap(),
        vec![
            wire_message("live:req", 265),
            wire_message("retained:req", 42000),
        ],
        vec![1_usize, usize::MAX],
        None::<()>,
        None::<()>,
        None::<()>,
    );
    let legacy = TempDir::new().unwrap();
    let shard = legacy
        .path()
        .join(CACHE_SHARD_DIRNAME)
        .join("claude")
        .join(format!(
            "shard-{:02x}.bin",
            legacy_shard_index("claude", &CachedPath::from_path(&path))
        ));
    fs::create_dir_all(shard.parent().unwrap()).unwrap();
    let mut envelope = CachedShardEnvelope {
        format_version: 10,
        parser_namespace: "claude".into(),
        parser_version: 2,
        payload: encode(&vec![entry]).unwrap(),
    };
    let original = encode(&envelope).unwrap();
    fs::write(&shard, &original).unwrap();
    fs::remove_file(&path).unwrap();
    let target = TempDir::new().unwrap();
    crate::usage_migration::prepare(target.path(), &[legacy.path().to_path_buf()]).unwrap();
    let cache = SourceMessageCache::open_from_root(Some(target.path()));
    let metadata = cache.retention_metadata(identity, &path).unwrap().unwrap();
    let found = rows(&cache, &metadata);
    assert_eq!(
        found
            .iter()
            .map(|row| (row.0.tokens.input, row.1, row.2))
            .collect::<Vec<_>>(),
        vec![(265, false, false), (42000, true, false)]
    );
    for (message, _, _) in found {
        assert_eq!(
            (message.tokens.cache_write_1h, message.tokens.reasoning),
            (17, 3)
        );
        assert_eq!(
            (
                message.timestamp,
                message.message_count,
                message.duration_ms
            ),
            (100, 2, Some(123))
        );
        assert!(message.is_turn_start);
        assert!(!message.input_tokens_are_cache_split);
    }
    crate::usage_migration::prepare(target.path(), &[legacy.path().to_path_buf()]).unwrap();
    assert_eq!(fs::read(&shard).unwrap(), original);
    assert_eq!(
        rows(&cache, &metadata)
            .iter()
            .map(|row| row.0.tokens.input)
            .sum::<i64>(),
        42265
    );
    envelope.format_version = 11;
    let unsupported = encode(&envelope).unwrap();
    fs::write(&shard, &unsupported).unwrap();
    let rejected = TempDir::new().unwrap();
    assert!(
        crate::usage_migration::prepare(rejected.path(), &[legacy.path().to_path_buf()]).is_err()
    );
    assert!(!rejected
        .path()
        .join("source-message-cache.sqlite3")
        .exists());
    assert_eq!(fs::read(shard).unwrap(), unsupported);
}

/// Source-free shard migration: a legacy file shard imports rows into a
/// shard-only staged store with the native source absent; originals are
/// preserved (no unlink) and re-import is idempotent via evidence.
#[test]
fn durable_shard_only_import_preserves_originals_and_serves_facts() {
    use crate::message_cache::import_legacy_shards_on;
    // Real native source: seed a legacy shard envelope for its path, then
    // delete the source so the shard is the only remaining copy.
    let sources = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let entry = CachedSourceEntry::new(
        identity,
        &path,
        SourceFingerprint::from_path(&path).unwrap(),
        vec![message("shard:req", 42)],
        Vec::new(),
        None,
    );
    let legacy = TempDir::new().unwrap();
    let shard = seed_legacy(legacy.path(), identity, &path, &[entry]);
    std::fs::remove_file(&path).unwrap();
    // Shard-only staged store: fresh DB file, no mc tables yet.
    let staged = TempDir::new().unwrap();
    let db_path = staged.path().join("staged.sqlite3");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    import_legacy_shards_on(&db, legacy.path()).unwrap();
    assert!(
        shard.exists(),
        "non-retiring import never unlinks the original"
    );
    // Facts servable without the native source, through frozen codecs.
    let mut inputs = Vec::new();
    let mut statement = db
        .prepare("SELECT message FROM mc_rows ORDER BY ordinal")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    while let Some(row) = rows.next().unwrap() {
        let bytes: Vec<u8> = row.get(0).unwrap();
        let format: u32 = db
            .query_row(
                "SELECT format_version FROM mc_versions LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let decoded =
            crate::message_cache::decode_stored_message_for_query(format, &bytes).unwrap();
        inputs.push(decoded.tokens.input);
    }
    assert_eq!(inputs, vec![42]);
    // Idempotent: second import is a no-op over recorded evidence.
    import_legacy_shards_on(&db, legacy.path()).unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM mc_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

/// A valid empty shard (supported envelope, zero entries) imports as a
/// receipt without requiring rows: evidence recorded, original kept,
/// re-import idempotent.
#[test]
fn durable_empty_shard_import_records_receipt_without_rows() {
    use crate::message_cache::import_legacy_shards_on;
    let sources = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Claude);
    let entry = CachedSourceEntry::new(
        identity,
        &path,
        SourceFingerprint::from_path(&path).unwrap(),
        Vec::new(),
        Vec::new(),
        None,
    );
    let legacy = TempDir::new().unwrap();
    let shard = seed_legacy(legacy.path(), identity, &path, &[entry]);
    std::fs::remove_file(&path).unwrap();
    let staged = TempDir::new().unwrap();
    let db_path = staged.path().join("staged.sqlite3");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    import_legacy_shards_on(&db, legacy.path()).unwrap();
    assert!(
        shard.exists(),
        "non-retiring import never unlinks the original"
    );
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM mc_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "empty receipt holds no rows");
    import_legacy_shards_on(&db, legacy.path()).unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM mc_rows", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn non_warm_generation_preserves_collected_facts_after_source_loss() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "amp.jsonl");
    let identity = CacheIdentity::for_client(ClientId::Amp);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(
        &cache,
        identity,
        &path,
        &[message("a:req", 10), message("b:req", 50)],
    );
    fs::write(&path, b"changed\n").unwrap();
    let observed = cache.retention_metadata(identity, &path).unwrap();
    let metadata = cache
        .write_source(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            observed.as_ref(),
            None,
            None,
            &mut |_| {},
            &mut |_, _| Ok(()),
            |writer| {
                writer.push(&message("c:req", 70), false, false)?;
                writer.cacheable = false;
                Ok(())
            },
        )
        .unwrap();
    assert!(!metadata.cacheable);
    assert!(cache.metadata(identity, &path).unwrap().is_none());
    fs::remove_file(&path).unwrap();
    drop(cache);
    let reopened = SourceMessageCache::open_from_root(Some(root.path()));
    let durable = reopened
        .retention_metadata(identity, &path)
        .unwrap()
        .unwrap();
    let mut inputs: Vec<_> = rows(&reopened, &durable)
        .into_iter()
        .map(|row| row.0.tokens.input)
        .collect();
    inputs.sort();
    assert_eq!(inputs, vec![10, 50, 70]);
}

#[test]
fn older_shard_adds_missing_facts_without_overwriting_current_observations() {
    let sources = TempDir::new().unwrap();
    let root = TempDir::new().unwrap();
    let path = source(sources.path(), "session.jsonl");
    let identity = CacheIdentity::for_client(ClientId::OpenCode);
    let cache = SourceMessageCache::open_from_root(Some(root.path()));
    publish(&cache, identity, &path, &[message("a:req", 10)]);
    let legacy = CachedSourceEntry::new(
        identity,
        &path,
        SourceFingerprint::from_path(&path).unwrap(),
        vec![message("a:req", 99), message("b:req", 50)],
        Vec::new(),
        None,
    );
    seed_legacy(root.path(), identity, &path, &[legacy]);
    cache
        .with_database(None, |db| import_legacy_shards_on(db, root.path()))
        .unwrap();
    fs::remove_file(&path).unwrap();
    let durable = cache.retention_metadata(identity, &path).unwrap().unwrap();
    let mut inputs: Vec<_> = rows(&cache, &durable)
        .into_iter()
        .map(|row| row.0.tokens.input)
        .collect();
    inputs.sort();
    assert_eq!(inputs, vec![10, 50]);
}

#[test]
fn source_free_shard_migration_serves_reports_after_legacy_cache_removal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for expected in [0, 42] {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let project = home.join(".claude/projects/proof");
        fs::create_dir_all(&project).unwrap();
        let path = source(&project, "session.jsonl");
        let identity = CacheIdentity::for_client(ClientId::Claude);
        let messages = if expected == 0 {
            Vec::new()
        } else {
            vec![message("shard:req", expected)]
        };
        let entry = CachedSourceEntry::new(
            identity,
            &path,
            SourceFingerprint::from_path(&path).unwrap(),
            messages,
            Vec::new(),
            None,
        );
        let legacy = root.path().join("cache");
        let shard = seed_legacy(&legacy, identity, &path, &[entry]);
        fs::remove_dir_all(home.join(".claude")).unwrap();
        let config = root.path().join("config");
        let data = root.path().join("data");
        let settings = crate::scanner::ScannerSettings::default();
        let fingerprint = crate::RemoteSourceContextV1::preview_fingerprint(
            &home, &config, &data, &legacy, &settings,
        )
        .unwrap();
        let remote = crate::RemoteSourceContextV1::new(
            &home,
            &config,
            &data,
            &legacy,
            settings,
            crate::RemoteSourceScopeTokenV1::new(fingerprint, 1),
        )
        .unwrap();
        let context = remote.local();
        let options = crate::ReportOptions {
            clients: Some(vec!["claude".into()]),
            ..Default::default()
        };
        let first = runtime
            .block_on(crate::get_model_report_with_source_context(
                context,
                options.clone(),
            ))
            .unwrap();
        assert_eq!(first.total_input, expected);
        assert_eq!(first.total_messages, i32::from(expected != 0));
        assert!(shard.exists());
        fs::remove_dir_all(&legacy).unwrap();
        let second = runtime
            .block_on(crate::get_model_report_with_source_context(
                context, options,
            ))
            .unwrap();
        assert_eq!(second.total_input, expected);
        assert_eq!(second.total_messages, first.total_messages);
    }
}
