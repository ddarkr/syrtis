use super::*;
use tempfile::TempDir;

fn codec_candidate() -> OpenCodeCandidate {
    opencode::parse_opencode_candidate(
        (
            "row".into(),
            "session".into(),
            r#"{"role":"assistant","modelID":"gpt-4o","providerID":"openai","sessionID":"session","tokens":{"input":3,"output":2,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1700000000000,"completed":1700000000010}}"#.into(),
            None,
        ),
        true,
    )
    .unwrap()
}

#[test]
fn candidate_codec_preserves_exact_typed_bytes_and_rejects_corruption() {
    let mut candidate = codec_candidate();
    let old = encode(&candidate).unwrap();
    let compact = encode_candidate(&candidate).unwrap();
    assert!(
        compact.len() < old.len(),
        "{} >= {}",
        compact.len(),
        old.len()
    );
    assert_eq!(encode(&decode_candidate(&compact).unwrap()).unwrap(), old);
    assert_eq!(encode(&decode_candidate(&old).unwrap()).unwrap(), old);
    candidate.message.timestamp = i64::MIN;
    candidate.message.duration_ms = Some(i64::MAX);
    candidate.message.tokens.input = i64::MIN;
    candidate.message.tokens.output = i64::MAX;
    candidate.message.workspace_key = Some(String::new());
    candidate.message.workspace_label = None;
    candidate.message.dedup_key = Some(String::new());
    candidate.message.dedup_aliases = vec!["b".into(), "".into(), "b".into(), "a".into()];
    candidate.embedded_id = Some(String::new());
    // Fingerprint bit fields are integer facts, including nonfinite original
    // floats; the persisted codec must not normalize them through JSON/f64.
    candidate.fingerprint = decode(
        &encode(&(
            u64::MAX,
            Some(0x8000_0000_0000_0000u64),
            String::new(),
            "provider".to_string(),
            i64::MIN,
            i64::MAX,
            -1i64,
            0i64,
            1i64,
            Some(String::new()),
        ))
        .unwrap(),
    )
    .unwrap();
    for bits in [
        0u64,
        1u64 << 63,
        f64::INFINITY.to_bits(),
        0x7ff8_0000_0000_0042,
    ] {
        candidate.message.cost = f64::from_bits(bits);
        let expected = encode(&candidate).unwrap();
        let compact = encode_candidate(&candidate).unwrap();
        assert_eq!(
            encode(&decode_candidate(&compact).unwrap()).unwrap(),
            expected
        );
        assert!(decode_candidate(&compact[..compact.len() - 1]).is_err());
        let mut trailing = compact.clone();
        trailing.push(0);
        assert!(decode_candidate(&trailing).is_err());
        let mut unknown = compact;
        unknown[8] = 2;
        assert!(decode_candidate(&unknown).is_err());
    }
}

#[test]
fn explicit_candidate_compaction_rolls_back_corrupt_and_full_disk_rows() {
    for full in [false, true] {
        let conn = Connection::open_in_memory().unwrap();
        schema(&conn).unwrap();
        let old = encode(&codec_candidate()).unwrap();
        conn.execute("UPDATE oc_meta SET version=2", []).unwrap();
        for ordinal in 0..3 {
            conn.execute("INSERT INTO oc_rows VALUES(1,?1,?2)", params![ordinal, old])
                .unwrap();
        }
        if full {
            conn.execute_batch(
                "CREATE TABLE fill(payload BLOB);
                CREATE TRIGGER fail_candidate BEFORE UPDATE ON oc_rows WHEN old.ordinal=1
                BEGIN INSERT INTO fill VALUES(zeroblob(1048576)); END;",
            )
            .unwrap();
            let pages: i64 = conn
                .pragma_query_value(None, "page_count", |r| r.get(0))
                .unwrap();
            conn.pragma_update(None, "max_page_count", pages).unwrap();
        } else {
            conn.execute("UPDATE oc_rows SET candidate=x'00' WHERE ordinal=1", [])
                .unwrap();
        }
        let error = compact_opencode_candidates(&conn).unwrap_err();
        if full {
            assert!(error.contains("DiskFull"), "{error}");
            let count: i64 = conn
                .query_row("SELECT count(*) FROM fill", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
            conn.execute_batch("DROP TRIGGER fail_candidate").unwrap();
        }
        assert_eq!(
            conn.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let first: Vec<u8> = conn
            .query_row("SELECT candidate FROM oc_rows WHERE ordinal=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(first, old);
        conn.execute("UPDATE oc_rows SET candidate=?1 WHERE ordinal=1", [&old])
            .unwrap();
        compact_opencode_candidates(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        for ordinal in 0..3 {
            let bytes: Vec<u8> = conn
                .query_row(
                    "SELECT candidate FROM oc_rows WHERE ordinal=?1",
                    [ordinal],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(encode(&decode_candidate(&bytes).unwrap()).unwrap(), old);
        }
    }
}

#[test]
fn opencode_group_probe_uses_one_snapshot_across_atomic_publication() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("usage.sqlite");
    let reader = Connection::open(&path).unwrap();
    reader.pragma_update(None, "journal_mode", "WAL").unwrap();
    let publisher = Connection::open(&path).unwrap();
    usage_store_schema::GROUP_PROBE_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            // Commit the complete group while the other connection's schema
            // inspection still owns its pre-publication SQLite read snapshot.
            usage_store_schema::ensure_opencode(&publisher).unwrap();
        }));
    });
    usage_store_schema::ensure_opencode(&reader).unwrap();
    assert_eq!(
        reader
            .query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    // Warm admission must remain read-only, including avoiding BEGIN IMMEDIATE.
    reader.pragma_update(None, "query_only", true).unwrap();
    usage_store_schema::ensure_opencode(&reader).unwrap();
}

#[test]
fn opencode_cold_probe_rechecks_competing_partial_and_unsupported_publications() {
    for partial in [true, false] {
        let root = TempDir::new().unwrap();
        let path = root.path().join("usage.sqlite");
        let reader = Connection::open(&path).unwrap();
        reader.pragma_update(None, "journal_mode", "WAL").unwrap();
        let publisher = Connection::open(&path).unwrap();
        usage_store_schema::GROUP_PROBE_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if partial {
                    publisher.execute_batch("BEGIN IMMEDIATE; CREATE TABLE oc_meta(version INTEGER NOT NULL); INSERT INTO oc_meta VALUES(2); COMMIT;").unwrap();
                } else {
                    usage_store_schema::ensure_opencode(&publisher).unwrap();
                    publisher.execute("UPDATE oc_meta SET version=4", []).unwrap();
                }
            }));
        });
        let error = usage_store_schema::ensure_opencode(&reader).unwrap_err();
        assert!(
            error.contains(if partial { "incomplete" } else { "unsupported" }),
            "{error}"
        );
        // Neither the cold recheck nor a later warm probe repairs the publisher.
        assert!(usage_store_schema::ensure_opencode(&reader)
            .unwrap_err()
            .contains(if partial { "incomplete" } else { "unsupported" }));
        let tables: i64 = reader
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name LIKE 'oc_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, if partial { 1 } else { 6 });
        assert_eq!(
            reader
                .query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            if partial { 2 } else { 4 }
        );
    }
}

fn legacy(conn: &Connection) -> Vec<Vec<u8>> {
    configure_connection(conn).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute_batch("CREATE TABLE oc_meta(version INTEGER NOT NULL); INSERT INTO oc_meta VALUES(1);
        CREATE TABLE oc_generations(generation BLOB PRIMARY KEY,path BLOB NOT NULL,fingerprint BLOB NOT NULL,kind INTEGER NOT NULL);
        CREATE TABLE oc_sources(path BLOB PRIMARY KEY,fingerprint BLOB NOT NULL,kind INTEGER NOT NULL,generation BLOB NOT NULL);
        CREATE TABLE oc_rows(source BLOB NOT NULL,ordinal INTEGER NOT NULL,candidate BLOB NOT NULL,PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE TABLE oc_messages(source BLOB NOT NULL,ordinal INTEGER NOT NULL,fingerprint BLOB NOT NULL,embedded_id TEXT,v1_fallback INTEGER NOT NULL,workspace_conflict INTEGER NOT NULL,message BLOB NOT NULL,payload BLOB NOT NULL,timestamp INTEGER NOT NULL,provider INTEGER NOT NULL,primary_key TEXT,PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE INDEX oc_fingerprint ON oc_messages(source,fingerprint,ordinal);
        CREATE INDEX oc_fingerprint_id ON oc_messages(source,fingerprint,embedded_id,ordinal);
        CREATE TABLE oc_aliases(source BLOB NOT NULL,ordinal INTEGER NOT NULL,position INTEGER NOT NULL,key TEXT NOT NULL,PRIMARY KEY(source,ordinal,key)) WITHOUT ROWID;
        CREATE INDEX oc_alias_order ON oc_aliases(source,ordinal,position);
        CREATE TABLE sq_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
        INSERT INTO sq_meta VALUES('schema_version',2),('store_epoch',x'000102030405060708090a0b0c0d0e0f');
        CREATE TABLE sq_scopes(id INTEGER PRIMARY KEY,scope_key BLOB,current_generation INTEGER);
        INSERT INTO sq_scopes VALUES(9,x'7777',31);
        CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,scope_id INTEGER,complete INTEGER,selection INTEGER);
        INSERT INTO sq_scope_generations VALUES(31,9,1,0);
        CREATE TABLE sq_scope_sources(scope_generation INTEGER,kind TEXT,lane TEXT,source_order INTEGER,path BLOB,cache_source_id INTEGER,cache_generation INTEGER,generation BLOB,tag INTEGER NOT NULL DEFAULT 0,headless INTEGER NOT NULL DEFAULT 0,workspace BLOB,workspace_label BLOB,phase INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scope_generation,kind,lane,source_order)) WITHOUT ROWID;
        CREATE INDEX sq_scope_opencode_versions ON sq_scope_sources(kind,path,generation);
        CREATE TABLE sq_indexes(scope_generation INTEGER PRIMARY KEY,version INTEGER);
        INSERT INTO sq_indexes VALUES(31,1);
        CREATE TABLE sq_decisions(scope_generation INTEGER,rank INTEGER,lane TEXT,timestamp INTEGER,cache_source_id INTEGER,cache_generation INTEGER,cache_ordinal INTEGER,cache_format INTEGER,oc_source BLOB,oc_ordinal INTEGER,retained INTEGER NOT NULL DEFAULT 0,global_storage INTEGER NOT NULL DEFAULT 0,workspace BLOB,workspace_label BLOB,model TEXT,headless INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(scope_generation,rank)) WITHOUT ROWID;
        CREATE INDEX sq_decision_time ON sq_decisions(scope_generation,timestamp,rank);
        CREATE TABLE du_reset(epoch BLOB); INSERT INTO du_reset VALUES(x'000102030405060708090a0b0c0d0e0f');
        CREATE TABLE mc_fixture(digest BLOB); INSERT INTO mc_fixture VALUES(x'00ff0102');").unwrap();
    conn.execute_batch("CREATE TABLE mc_meta(key TEXT PRIMARY KEY,value INTEGER); INSERT INTO mc_meta VALUES('schema_version',1);
        CREATE TABLE mc_sources(id INTEGER PRIMARY KEY,namespace TEXT,scope BLOB,path BLOB,active INTEGER,generation INTEGER); INSERT INTO mc_sources VALUES(4,'fixture',x'00',x'01',1,12);
        CREATE TABLE mc_versions(source_id INTEGER,generation INTEGER,parser_version INTEGER,format_version INTEGER,fingerprint BLOB,resume BLOB,message_count INTEGER,retained_count INTEGER,fallback_count INTEGER,retained_digest BLOB,cacheable INTEGER,codex_turn_count INTEGER,codex_turn_digest BLOB,PRIMARY KEY(source_id,generation)) WITHOUT ROWID;
        INSERT INTO mc_versions VALUES(4,12,1,5,x'01',NULL,1,0,0,x'02',1,0,x'03');
        CREATE TABLE mc_rows(source_id INTEGER,generation INTEGER,ordinal INTEGER,dedup_key TEXT,retained INTEGER,fallback INTEGER,timestamp INTEGER,message BLOB NOT NULL,digest BLOB NOT NULL,session_id TEXT,model_id TEXT NOT NULL,client TEXT,provider_id TEXT,workspace BLOB NOT NULL,workspace_label BLOB NOT NULL,PRIMARY KEY(source_id,generation,ordinal)) WITHOUT ROWID;
        CREATE TABLE mc_codex_turn_ids(source_id INTEGER,generation INTEGER,turn_id TEXT);
        CREATE TABLE mc_migrations(path BLOB,digest BLOB,source_count INTEGER);
        CREATE TABLE mc_migration_versions(path BLOB,digest BLOB,source_id INTEGER,generation INTEGER);
        CREATE TABLE mc_namespaces(namespace TEXT,signature BLOB);").unwrap();
    let none = encode(&Option::<String>::None).unwrap();
    conn.execute("INSERT INTO mc_rows VALUES(4,12,6,'key',0,0,124,x'00ff01',x'fe0200','session','model','claude','provider',?1,?1)", [&none]).unwrap();
    conn.execute("INSERT INTO sq_decisions(scope_generation,rank,lane,timestamp,cache_source_id,cache_generation,cache_ordinal,cache_format,workspace,workspace_label,model) VALUES(31,23,'claude',124,4,12,6,5,?1,?1,'model')", [&none]).unwrap();
    let mut descriptors = Vec::new();
    // Current, pinned superseded, and source-free retained generation; both kinds.
    for n in 0..3u8 {
        let path = vec![if n < 2 { 0 } else { n }];
        let fingerprint = vec![255, n];
        let kind = n != 2;
        let descriptor = encode(&(&path, &fingerprint, kind)).unwrap();
        conn.execute(
            "INSERT INTO oc_generations VALUES(?1,?2,?3,?4)",
            params![descriptor, path, fingerprint, kind],
        )
        .unwrap();
        if n == 0 {
            conn.execute(
                "INSERT INTO oc_sources VALUES(?1,?2,?3,?4)",
                params![path, fingerprint, kind, descriptor],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO oc_rows VALUES(?1,7,x'ff0001')", [&descriptor])
            .unwrap();
        conn.execute("INSERT INTO oc_messages VALUES(?1,7,x'fe0100','id',1,1,x'010200',x'0003ff',123,1,'primary')", [&descriptor]).unwrap();
        conn.execute(
            "INSERT INTO oc_aliases VALUES(?1,7,-1,'primary'),(?1,7,8,'alias')",
            [&descriptor],
        )
        .unwrap();
        if n != 2 {
            conn.execute(
                "INSERT INTO sq_scope_sources(scope_generation,kind,lane,source_order,path,generation) VALUES(31,'opencode-sqlite','opencode',?1,?2,?3)",
                params![n, path, descriptor],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sq_decisions(scope_generation,rank,lane,timestamp,oc_source,oc_ordinal,workspace,workspace_label,model) VALUES(31,?1,'opencode',123,?2,7,?3,?3,'oc-model')",
                params![n as i64 + 17, descriptor, none],
            )
            .unwrap();
        }
        descriptors.push(descriptor);
    }
    descriptors
}

fn assert_legacy(conn: &Connection) {
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM oc_rows WHERE typeof(source)='blob'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    assert!(!conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name LIKE 'normalized_%')",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}

#[test]
fn legacy_migration_preserves_exact_facts_references_and_identity() {
    let conn = Connection::open_in_memory().unwrap();
    let descriptors = legacy(&conn);
    admit_schema(&conn).unwrap();
    for descriptor in descriptors {
        let generation: i64 = conn
            .query_row(
                "SELECT generation FROM oc_generations WHERE descriptor=?1",
                [descriptor],
                |r| r.get(0),
            )
            .unwrap();
        let values: (Vec<u8>,Vec<u8>,Vec<u8>,Vec<u8>,String,i64) = conn.query_row("SELECT r.candidate,m.fingerprint,m.message,m.payload,a.key,a.position FROM oc_rows r JOIN oc_messages m USING(source,ordinal) JOIN oc_aliases a USING(source,ordinal) WHERE r.source=?1 AND a.position=8",[generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(
            values,
            (
                vec![255, 0, 1],
                vec![254, 1, 0],
                vec![1, 2, 0],
                vec![0, 3, 255],
                "alias".into(),
                8
            )
        );
    }
    assert_eq!(
        conn.query_row(
            "SELECT hex(value) FROM sq_meta WHERE key='store_epoch'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "000102030405060708090A0B0C0D0E0F"
    );
    assert_eq!(
        conn.query_row("SELECT hex(epoch) FROM du_reset", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "000102030405060708090A0B0C0D0E0F"
    );
    assert_eq!(
        conn.query_row("SELECT hex(digest) FROM mc_fixture", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "00FF0102"
    );
    assert_eq!(
        conn.query_row(
            "SELECT sum(rank) FROM sq_decisions WHERE typeof(oc_source)='integer'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        35
    );
    let metadata: (Vec<u8>, Vec<u8>, String, Vec<u8>, Vec<u8>, i64) = conn.query_row(
        "SELECT COALESCE(d.workspace,m.workspace),COALESCE(d.workspace_label,m.workspace_label),COALESCE(d.model,m.model_id),m.message,m.digest,d.rank
         FROM sq_decisions d JOIN mc_rows m ON m.source_id=d.cache_source_id AND m.generation=d.cache_generation AND m.ordinal=d.cache_ordinal",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
    let none = encode(&Option::<String>::None).unwrap();
    assert_eq!(
        metadata,
        (
            none.clone(),
            none.clone(),
            "model".into(),
            vec![0, 255, 1],
            vec![254, 2, 0],
            23
        )
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM sq_decisions WHERE cache_source_id IS NOT NULL AND workspace IS NULL AND workspace_label IS NULL AND model IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(conn.query_row("SELECT count(*) FROM sq_decisions WHERE oc_source IS NOT NULL AND workspace=?1 AND workspace_label=?1 AND model='oc-model'", [none], |r| r.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(
        conn.query_row(
            "SELECT current_generation FROM sq_scopes WHERE id=9",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        31
    );
    let before: i64 = conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='oc_generations'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    admit_schema(&conn).unwrap();
    conn.execute("DELETE FROM oc_generations WHERE generation=?1", [before])
        .unwrap();
    conn.execute(
        "INSERT INTO oc_generations(descriptor,path,fingerprint,kind) VALUES(x'ab',x'ab',x'ab',0)",
        [],
    )
    .unwrap();
    assert!(conn.last_insert_rowid() > before);
}

#[test]
fn migration_failure_rolls_back_ddl_rows_and_versions() {
    let conn = Connection::open_in_memory().unwrap();
    legacy(&conn);
    usage_store_schema::MIGRATION_FAILURE.with(|f| f.set(true));
    let result = admit_schema(&conn);
    usage_store_schema::MIGRATION_FAILURE.with(|f| f.set(false));
    assert!(result.is_err());
    assert_legacy(&conn);
    admit_schema(&conn).unwrap();
}

#[test]
fn sqlite_full_migration_rolls_back_all_legacy_data_and_retries() {
    let root = TempDir::new().unwrap();
    let conn = Connection::open(root.path().join("full.sqlite")).unwrap();
    legacy(&conn);
    let snapshot = || {
        let mut objects = conn
            .prepare("SELECT type,name,sql FROM sqlite_master ORDER BY type,name")
            .unwrap();
        let objects: Vec<(String, String, Option<String>)> = objects
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut schema = conn
            .prepare("SELECT name,sql FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<(String, String)> = schema
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let data = tables
            .into_iter()
            .map(|(name, definition)| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();
                let count = stmt.column_count();
                let mut rows: Vec<Vec<rusqlite::types::Value>> = stmt
                    .query_map([], |r| (0..count).map(|column| r.get(column)).collect())
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                rows.sort_by_key(|row| format!("{row:?}"));
                (name, definition, rows)
            })
            .collect::<Vec<_>>();
        (objects, data)
    };
    let before = snapshot();
    let pages: i64 = conn
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    conn.pragma_update(None, "max_page_count", pages).unwrap();
    let error = admit_schema(&conn).unwrap_err();
    assert!(
        error.contains("SQLite DiskFull, extended code 13"),
        "{error}"
    );
    assert_legacy(&conn);
    assert_eq!(snapshot(), before);
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "application_id", |r| r.get(0))
            .unwrap(),
        APPLICATION_ID
    );
    conn.pragma_update(None, "max_page_count", pages + 1024)
        .unwrap();
    admit_schema(&conn).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM sq_decisions WHERE cache_source_id=4 AND workspace IS NULL AND workspace_label IS NULL AND model IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn optimize_compacts_existing_v2_decisions_without_creating_recovery() {
    let root = TempDir::new().unwrap();
    let usage = root.path();
    secure_directory(usage).unwrap();
    let conn = Connection::open(usage.join(DATABASE)).unwrap();
    legacy(&conn);
    admit_schema(&conn).unwrap();
    // The legacy migration fixture deliberately uses opaque candidate bytes to
    // test byte-preserving reference normalization. Optimize also decodes facts,
    // so seed a real legacy candidate for this maintenance-specific fixture.
    let candidate = encode(&codec_candidate()).unwrap();
    conn.execute("UPDATE oc_rows SET candidate=?1", [&candidate])
        .unwrap();
    // Model an already-published v2 database that still has physical copies.
    let none = encode(&Option::<String>::None).unwrap();
    conn.execute("UPDATE sq_decisions SET workspace=?1,workspace_label=?1,model='model' WHERE cache_source_id=4", [&none]).unwrap();
    crate::disk_usage::compact(&conn, usage, u64::MAX).unwrap();
    assert!(!usage.join(RECOVERY).exists());
    {
        let db = Connection::open(usage.join(DATABASE)).unwrap();
        let mut candidates = db
            .prepare("SELECT candidate FROM oc_rows ORDER BY source,ordinal")
            .unwrap();
        let rows = candidates
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .unwrap();
        for row in rows {
            assert_eq!(
                encode(&decode_candidate(&row.unwrap()).unwrap()).unwrap(),
                candidate
            );
        }
        assert_eq!(db.query_row("SELECT count(*) FROM sq_decisions WHERE cache_source_id=4 AND rank=23 AND workspace IS NULL AND workspace_label IS NULL AND model IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        let facts: (Vec<u8>,Vec<u8>,Vec<u8>,Vec<u8>,String) = db.query_row(
            "SELECT m.message,m.digest,COALESCE(d.workspace,m.workspace),COALESCE(d.workspace_label,m.workspace_label),COALESCE(d.model,m.model_id) FROM sq_decisions d JOIN mc_rows m ON m.source_id=d.cache_source_id AND m.generation=d.cache_generation AND m.ordinal=d.cache_ordinal",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(
            facts,
            (
                vec![0, 255, 1],
                vec![254, 2, 0],
                none.clone(),
                none.clone(),
                "model".into()
            )
        );
        assert_eq!(
            db.query_row(
                "SELECT hex(value) FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "000102030405060708090A0B0C0D0E0F"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM oc_rows", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
}

#[test]
fn migration_rejects_bad_descriptor_dangling_mapping_and_partial_schema() {
    for mutation in [
        "UPDATE oc_generations SET fingerprint=x'ee'",
        "UPDATE sq_decisions SET oc_source=x'ee'",
        "DROP TABLE oc_aliases",
        "UPDATE sq_meta SET value=99 WHERE key='schema_version'",
    ] {
        let conn = Connection::open_in_memory().unwrap();
        legacy(&conn);
        conn.execute_batch(mutation).unwrap();
        assert!(admit_schema(&conn).is_err(), "{mutation}");
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn migration_serializes_two_openers_and_reports_contention_cleanly() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("fixture.db");
    let conn = Connection::open(&path).unwrap();
    legacy(&conn);
    conn.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;")
        .unwrap();
    let other = Connection::open(&path).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    assert!(admit_schema(&other).unwrap_err().contains("SQLite"));
    assert_legacy(&conn);
    conn.execute_batch("ROLLBACK").unwrap();
    drop(other);
    drop(conn);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let conn = Connection::open(path).unwrap();
                configure_connection(&conn).unwrap();
                barrier.wait();
                admit_schema(&conn).unwrap();
                conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), 2);
    }
}

#[test]
fn old_recovery_migrates_staging_and_preserves_reset_barrier() {
    let root = TempDir::new().unwrap();
    secure_directory(root.path()).unwrap();
    let recovery = Connection::open(root.path().join(RECOVERY)).unwrap();
    legacy(&recovery);
    drop(recovery);
    write_reset_barrier(root.path(), &[0; 16]).unwrap();
    assert!(recover_database(root.path()).is_err());
    assert!(!root.path().join(DATABASE).exists());
    write_reset_barrier(root.path(), &(0..16u8).collect::<Vec<_>>()).unwrap();
    recover_database(root.path()).unwrap();
    let canonical = open_database(root.path()).unwrap();
    assert_eq!(
        canonical
            .query_row(
                "SELECT count(*) FROM oc_messages WHERE typeof(source)='integer'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    let old =
        Connection::open_with_flags(root.path().join(RECOVERY), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_legacy(&old);
}

#[test]
fn outer_only_legacy_database_upgrades_without_optional_facts() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute_batch(
        "CREATE TABLE mc_fixture(digest BLOB); INSERT INTO mc_fixture VALUES(x'ff');",
    )
    .unwrap();
    admit_schema(&conn).unwrap();
    assert_eq!(
        conn.query_row("SELECT hex(digest) FROM mc_fixture", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "FF"
    );
    schema(&conn).unwrap();
}

#[test]
fn generation_gc_uses_descriptor_order_not_allocation_order() {
    let conn = Connection::open_in_memory().unwrap();
    schema(&conn).unwrap();
    conn.execute_batch("CREATE TABLE sq_scope_sources(generation INTEGER,kind TEXT);
        INSERT INTO oc_generations(descriptor,path,fingerprint,kind) VALUES(x'ff',x'01',x'ff',1),(x'01',x'01',x'01',1),(x'02',x'01',x'02',1);
        INSERT INTO sq_scope_sources VALUES(3,'opencode-sqlite');").unwrap();
    discard_unreferenced_opencode_generations(&conn).unwrap();
    let ids = conn
        .prepare("SELECT generation FROM oc_generations ORDER BY generation")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn readonly_inspection_admits_legacy_without_migrating_and_rejects_future() {
    let root = TempDir::new().unwrap();
    let conn = Connection::open(root.path().join(DATABASE)).unwrap();
    legacy(&conn);
    drop(conn);
    let inspected = open_existing_database(root.path()).unwrap();
    assert_legacy(&inspected);
    drop(inspected);
    let conn = Connection::open(root.path().join(DATABASE)).unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();
    drop(conn);
    assert!(open_existing_database(root.path()).is_err());
    assert!(open_database(root.path()).is_err());
    let conn = Connection::open(root.path().join(DATABASE)).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        99
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM oc_rows", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn failed_staged_recovery_migration_does_not_publish_or_modify_old_backup() {
    let root = TempDir::new().unwrap();
    let conn = Connection::open(root.path().join(RECOVERY)).unwrap();
    legacy(&conn);
    drop(conn);
    usage_store_schema::MIGRATION_FAILURE.with(|f| f.set(true));
    let result = recover_database(root.path());
    usage_store_schema::MIGRATION_FAILURE.with(|f| f.set(false));
    assert!(result.is_err());
    assert!(!root.path().join(DATABASE).exists());
    let conn =
        Connection::open_with_flags(root.path().join(RECOVERY), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_legacy(&conn);
    recover_database(root.path()).unwrap();
}

fn inspect_managed_readonly(path: &Path) -> Result<(), String> {
    let writer = Connection::open(path).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let before = fs::read(path).unwrap();
    let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader.busy_timeout(Duration::ZERO).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    // Pin the catalog before the competing writer reserves the WAL writer slot.
    reader
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let result = validate_managed_schema_readonly(&reader);
    assert!(
        !reader.is_autocommit(),
        "validator ended the caller's snapshot"
    );
    reader.execute_batch("ROLLBACK").unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(writer);
    assert_eq!(
        fs::read(path).unwrap(),
        before,
        "read-only admission changed database bytes"
    );
    result
}

#[test]
fn managed_readonly_admits_legacy_and_current_without_rewriting_retained_facts() {
    for current in [false, true] {
        let root = TempDir::new().unwrap();
        let path = root.path().join(DATABASE);
        let conn = Connection::open(&path).unwrap();
        let descriptors = legacy(&conn);
        if current {
            admit_schema(&conn).unwrap();
        }
        drop(conn);
        inspect_managed_readonly(&path).unwrap();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        if !current {
            assert_legacy(&conn);
        }
        let source_free: i64 = conn.query_row(
            if current {
                "SELECT count(*) FROM oc_generations g JOIN oc_messages m ON m.source=g.generation WHERE g.descriptor=?1 AND NOT EXISTS(SELECT 1 FROM oc_sources s WHERE s.generation=g.generation)"
            } else {
                "SELECT count(*) FROM oc_generations g JOIN oc_messages m ON m.source=g.generation WHERE g.generation=?1 AND NOT EXISTS(SELECT 1 FROM oc_sources s WHERE s.generation=g.generation)"
            },
            [&descriptors[2]], |r| r.get(0),
        ).unwrap();
        assert_eq!(source_free, 1);
        assert_eq!(
            conn.query_row(
                "SELECT hex(value) FROM sq_meta WHERE key='store_epoch'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "000102030405060708090A0B0C0D0E0F"
        );
    }
}

#[test]
fn managed_readonly_admits_production_fresh_groups_and_absent_optional_groups() {
    let root = TempDir::new().unwrap();
    let conn = open_database(root.path()).unwrap();
    schema(&conn).unwrap();
    drop(conn);
    drop(crate::store_query::QueryState::open(root.path()).unwrap());
    crate::message_cache::SourceMessageCache::open_from_root(Some(root.path()))
        .ensure_open()
        .unwrap();
    inspect_managed_readonly(&root.path().join(DATABASE)).unwrap();
    for version in [1, 2] {
        let root = TempDir::new().unwrap();
        let path = root.path().join(DATABASE);
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        drop(conn);
        inspect_managed_readonly(&path).unwrap();
    }
}

#[test]
fn managed_scope_six_inspection_preserves_source_free_facts_without_writes() {
    let root = TempDir::new().unwrap();
    let path = root.path().join(DATABASE);
    let conn = Connection::open(&path).unwrap();
    let descriptors = legacy(&conn);
    admit_schema(&conn).unwrap();
    drop(conn);
    drop(crate::store_query::QueryState::open(root.path()).unwrap());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(conn.query_row(
        "SELECT value FROM sq_meta WHERE key='schema_version'", [],
        |r| r.get::<_, i64>(0),
    ).unwrap(), 6);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(conn);
    let before = fs::read(&path).unwrap();
    inspect_managed_readonly(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), before);
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(conn.query_row(
        "SELECT count(*) FROM oc_generations g JOIN oc_messages m ON m.source=g.generation WHERE g.descriptor=?1 AND NOT EXISTS(SELECT 1 FROM oc_sources s WHERE s.generation=g.generation)",
        [&descriptors[2]], |r| r.get::<_, i64>(0),
    ).unwrap(), 1);
}

#[test]
fn managed_scope_six_rejects_broken_registry_without_repair() {
    for mutation in [
        "DROP TABLE sq_scope_source_current",
        "DROP TABLE sq_scope_source_pins",
        "ALTER TABLE sq_scope_source_pins RENAME COLUMN phase TO invalid_phase",
        "DROP VIEW sq_scope_sources; CREATE VIEW sq_scope_sources AS SELECT * FROM sq_scope_source_pins",
        "PRAGMA user_version=1",
        "UPDATE sq_meta SET value=7 WHERE key='schema_version'",
    ] {
        let root = TempDir::new().unwrap();
        drop(crate::store_query::QueryState::open(root.path()).unwrap());
        let path = root.path().join(DATABASE);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(mutation).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        drop(conn);
        let before = fs::read(&path).unwrap();
        assert!(inspect_managed_readonly(&path).is_err(), "accepted {mutation}");
        assert_eq!(fs::read(&path).unwrap(), before);
        if mutation != "PRAGMA user_version=1" {
            assert!(
                crate::store_query::QueryState::open(root.path()).is_err(),
                "query admission accepted {mutation}"
            );
        }
    }
}

#[test]
fn managed_readonly_rejects_incomplete_and_future_groups_without_repair() {
    for current in [false, true] {
        for mutation in [
            "DROP TABLE oc_aliases",
            "DROP TABLE oc_aliases; CREATE VIEW oc_aliases AS SELECT * FROM oc_messages",
            "DROP TABLE sq_decisions",
            "DROP TABLE sq_indexes",
            "ALTER TABLE sq_decisions DROP COLUMN oc_ordinal",
            "ALTER TABLE sq_scope_sources DROP COLUMN phase",
            "ALTER TABLE sq_decisions RENAME COLUMN oc_ordinal TO old_oc_ordinal; ALTER TABLE sq_decisions ADD COLUMN oc_ordinal TEXT",
            "ALTER TABLE oc_rows RENAME COLUMN candidate TO old_candidate; ALTER TABLE oc_rows ADD COLUMN candidate TEXT",
            "ALTER TABLE sq_decisions RENAME COLUMN cache_format TO old_cache_format; ALTER TABLE sq_decisions ADD COLUMN cache_format TEXT",
            "UPDATE oc_meta SET version=99",
            "UPDATE sq_meta SET value=99 WHERE key='schema_version'",
            "UPDATE oc_generations SET fingerprint=x'ee'",
            "UPDATE oc_sources SET fingerprint=x'ee'",
            "DROP TABLE mc_codex_turn_ids",
        ] {
            let root = TempDir::new().unwrap();
            let path = root.path().join(DATABASE);
            let conn = Connection::open(&path).unwrap();
            legacy(&conn);
            if current {
                admit_schema(&conn).unwrap();
            }
            conn.execute_batch(mutation).unwrap();
            drop(conn);
            assert!(inspect_managed_readonly(&path).is_err(), "accepted broken outer-current={current}: {mutation}");
        }
    }
}

#[test]
fn managed_readonly_admits_historical_decisions_without_optional_cache_format() {
    for current in [false, true] {
        let root = TempDir::new().unwrap();
        let path = root.path().join(DATABASE);
        let conn = Connection::open(&path).unwrap();
        legacy(&conn);
        if current {
            admit_schema(&conn).unwrap();
        }
        conn.execute_batch("ALTER TABLE sq_decisions DROP COLUMN cache_format")
            .unwrap();
        drop(conn);
        inspect_managed_readonly(&path).unwrap();
    }
}

#[test]
fn old_candidate_recovery_upgrades_without_sources_or_changed_selection() {
    let root = TempDir::new().unwrap();
    let cache = root.path().join("usage");
    let absent_source = root.path().join("original.db");
    let conn = open_database(&cache).unwrap();
    schema(&conn).unwrap();
    let path = source_key(&absent_source).unwrap();
    let original = Connection::open(&absent_source).unwrap();
    original
        .execute_batch(
            "CREATE TABLE original_fact(value INTEGER); INSERT INTO original_fact VALUES(1);",
        )
        .unwrap();
    drop(original);
    let fingerprint = encode(&source_fingerprint(&absent_source, true).unwrap()).unwrap();
    fs::remove_file(&absent_source).unwrap();
    let descriptor = encode(&(&path, &fingerprint, true)).unwrap();
    conn.execute(
        "INSERT INTO oc_generations(descriptor,path,fingerprint,kind) VALUES(?1,?2,?3,1)",
        params![descriptor, path, fingerprint],
    )
    .unwrap();
    let generation = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO oc_sources VALUES(?1,?2,1,?3)",
        params![path, fingerprint, generation],
    )
    .unwrap();
    let mut originals = Vec::new();
    for ordinal in 0..3 {
        let mut candidate = codec_candidate();
        candidate.message.dedup_key = Some(format!("row-{ordinal}"));
        // A distinct original-float cohort rounds to the same output timestamp.
        if ordinal == 2 {
            candidate.fingerprint = decode(
                &encode(&(
                    1_700_000_000_000.25f64.to_bits(),
                    Some(1_700_000_000_010.0f64.to_bits()),
                    "gpt-4o".to_string(),
                    "openai".to_string(),
                    3i64,
                    2i64,
                    0i64,
                    0i64,
                    0i64,
                    Option::<String>::None,
                ))
                .unwrap(),
            )
            .unwrap();
        }
        let bytes = encode(&candidate).unwrap();
        conn.execute(
            "INSERT INTO oc_rows VALUES(?1,?2,?3)",
            params![generation, ordinal, bytes],
        )
        .unwrap();
        originals.push(bytes);
    }
    collapse_source(&conn, &generation, true).unwrap();
    conn.execute("UPDATE oc_meta SET version=2", []).unwrap();
    let output = |store: &Connection| {
        query_schema(store).unwrap();
        store
            .execute("INSERT INTO ocq_scope VALUES(?1,0,1)", [generation])
            .unwrap();
        let mut messages = Vec::new();
        visit_selected(
            store,
            OpenCodeSelectionMode::Materialized,
            true,
            &|_| true,
            &mut |message| messages.push(message.clone()),
        )
        .unwrap();
        encode(&messages).unwrap()
    };
    let expected = output(&conn);
    refresh_retained_recovery(&conn, &cache).unwrap();
    drop(conn);
    fs::remove_file(cache.join(DATABASE)).unwrap();
    recover_database(&cache).unwrap();
    let mut restored = open_database(&cache).unwrap();
    schema(&restored).unwrap();
    assert_eq!(
        sync_source(&mut restored, &absent_source, true, &mut |_, _| Ok(())).unwrap(),
        generation
    );
    assert_eq!(output(&restored), expected);
    for (ordinal, original) in originals.iter().enumerate() {
        let compact: Vec<u8> = restored
            .query_row(
                "SELECT candidate FROM oc_rows WHERE source=?1 AND ordinal=?2",
                params![generation, ordinal as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            &compact, original,
            "ordinary admission must not rewrite legacy candidates"
        );
        assert_eq!(
            encode(&decode_candidate(&compact).unwrap()).unwrap(),
            *original
        );
    }
    let old = Connection::open_with_flags(cache.join(RECOVERY), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    assert_eq!(
        old.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(old);
    let previous_recovery = fs::read(cache.join(RECOVERY)).unwrap();
    assert!(crate::disk_usage::compact(&restored, &cache, 0).is_err());
    assert_eq!(fs::read(cache.join(RECOVERY)).unwrap(), previous_recovery);
    restored
        .execute_batch(
            "CREATE TRIGGER reject_candidate_compaction BEFORE UPDATE ON oc_rows WHEN old.ordinal=1
         BEGIN SELECT RAISE(ABORT,'candidate compaction rejected'); END;",
        )
        .unwrap();
    assert!(crate::disk_usage::compact(&restored, &cache, u64::MAX).is_err());
    for (ordinal, original) in originals.iter().enumerate() {
        let actual: Vec<u8> = restored
            .query_row(
                "SELECT candidate FROM oc_rows WHERE source=?1 AND ordinal=?2",
                params![generation, ordinal as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            &actual, original,
            "failed Optimize must roll back the entire candidate rewrite"
        );
    }
    assert_eq!(fs::read(cache.join(RECOVERY)).unwrap(), previous_recovery);
    restored
        .execute_batch("DROP TRIGGER reject_candidate_compaction")
        .unwrap();
    crate::disk_usage::compact(&restored, &cache, u64::MAX).unwrap();
    assert_eq!(fs::read(cache.join(RECOVERY)).unwrap(), previous_recovery);
    for (ordinal, original) in originals.iter().enumerate() {
        let compact: Vec<u8> = restored
            .query_row(
                "SELECT candidate FROM oc_rows WHERE source=?1 AND ordinal=?2",
                params![generation, ordinal as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert!(compact.len() < original.len());
        assert_eq!(
            encode(&decode_candidate(&compact).unwrap()).unwrap(),
            *original
        );
    }
    assert_eq!(output(&restored), expected);
    drop(restored);
    fs::remove_file(cache.join(DATABASE)).unwrap();
    recover_database(&cache).unwrap();
    let reopened = open_database(&cache).unwrap();
    schema(&reopened).unwrap();
    assert_eq!(output(&reopened), expected);
    for (ordinal, original) in originals.iter().enumerate() {
        let compact: Vec<u8> = reopened
            .query_row(
                "SELECT candidate FROM oc_rows WHERE source=?1 AND ordinal=?2",
                params![generation, ordinal as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&compact, original);
        assert_eq!(
            encode(&decode_candidate(&compact).unwrap()).unwrap(),
            *original
        );
    }
}

#[test]
fn duplicate_candidate_collapse_writes_only_changed_authority_or_workspace() {
    let conn = Connection::open_in_memory().unwrap();
    schema(&conn).unwrap();
    conn.execute_batch(
        "CREATE TABLE update_count(n INTEGER); INSERT INTO update_count VALUES(0);
        CREATE TRIGGER count_collapse AFTER UPDATE ON oc_messages
        BEGIN UPDATE update_count SET n=n+1; END;",
    )
    .unwrap();
    let candidate = codec_candidate();
    for ordinal in 0..1030 {
        let mut row = decode_candidate(&encode_candidate(&candidate).unwrap()).unwrap();
        row.message.dedup_key = Some(format!("alias-{ordinal}"));
        if ordinal == 1028 {
            row.message.cost = 0.75;
            row.message.cost_source = CostSource::ProviderReported;
            row.message
                .set_workspace(Some("first".into()), Some("First".into()));
        }
        if ordinal == 1029 {
            row.message
                .set_workspace(Some("second".into()), Some("Second".into()));
        }
        conn.execute(
            "INSERT INTO oc_rows VALUES(1,?1,?2)",
            params![ordinal, encode_candidate(&row).unwrap()],
        )
        .unwrap();
    }
    collapse_source(&conn, &1, true).unwrap();
    assert_eq!(
        conn.query_row("SELECT n FROM update_count", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let bytes: Vec<u8> = conn
        .query_row("SELECT message FROM oc_messages", [], |r| r.get(0))
        .unwrap();
    let message: UnifiedMessage = decode(&bytes).unwrap();
    assert_eq!(message.cost.to_bits(), 0.75f64.to_bits());
    assert_eq!(message.cost_source, CostSource::ProviderReported);
    assert_eq!(message.workspace_key, None);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM oc_aliases", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1030
    );
    let mut statement = conn
        .prepare("SELECT key FROM oc_aliases WHERE position>=0 ORDER BY position")
        .unwrap();
    let aliases = statement
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        aliases,
        (1..1030)
            .map(|ordinal| format!("alias-{ordinal}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn opencode_transfer_over_batch_boundary_rolls_back_with_generation_owner() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("source.db");
    let source = Connection::open(&path).unwrap();
    source
        .execute_batch(
            "CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,data TEXT);
        CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT);
        INSERT INTO session VALUES('session','/workspace');",
        )
        .unwrap();
    let json = r#"{"role":"assistant","modelID":"gpt-4o","providerID":"openai","sessionID":"session","tokens":{"input":3,"output":2,"cache":{"read":0,"write":0}},"time":{"created":1700000000000}}"#;
    source
        .execute("INSERT INTO message VALUES('initial','session',?1)", [json])
        .unwrap();
    let mut store = open_database(&root.path().join("usage")).unwrap();
    schema(&store).unwrap();
    let previous = sync_source(&mut store, &path, true, &mut |_, _| Ok(())).unwrap();
    let before: Vec<u8> = store
        .query_row("SELECT candidate FROM oc_rows", [], |r| r.get(0))
        .unwrap();
    source.execute_batch("BEGIN").unwrap();
    for ordinal in 0..1100 {
        source
            .execute(
                "INSERT INTO message VALUES(?1,'session',?2)",
                params![format!("later-{ordinal}"), json],
            )
            .unwrap();
    }
    source.execute_batch("COMMIT").unwrap();
    assert!(sync_source(&mut store, &path, true, &mut |_, _| Err(
        "publication rejected".into()
    ))
    .is_err());
    assert_eq!(
        store
            .query_row("SELECT generation FROM oc_sources", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        previous
    );
    assert_eq!(
        store
            .query_row("SELECT count(*) FROM oc_generations", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .query_row("SELECT count(*) FROM oc_rows", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .query_row("SELECT candidate FROM oc_rows", [], |r| r
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        before
    );
}

#[cfg(unix)]
#[test]
fn detached_sidecar_metadata_is_optional_but_main_replacements_and_hardlinks_are_not() {
    use std::os::unix::fs::{symlink, MetadataExt, OpenOptionsExt};
    let root = TempDir::new().unwrap();
    let path = root.path().join("synthetic.sqlite-wal");
    // This is a synthetic non-SQLite inode. Retaining its descriptor lets the
    // fixture reproduce lstat's resolved-then-unlinked vnode deterministically,
    // without opening/closing any real SQLite-owned main or SHM descriptor.
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    fs::remove_file(&path).unwrap();
    let detached = file.metadata().unwrap();
    assert_eq!(detached.nlink(), 0);
    admit_file_metadata(&path, &detached, true).unwrap();
    assert!(admit_file_metadata(&path, &detached, false).is_err());
    assert!(!path.exists());

    let replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    assert!(
        admit_file_metadata(&path, &detached, true).is_err(),
        "a present replacement must never be excused by detached metadata"
    );
    admit_sidecar(&path).unwrap();
    let alias = root.path().join("hardlink");
    fs::hard_link(&path, &alias).unwrap();
    assert!(admit_sidecar(&path).unwrap_err().contains("ownership"));
    fs::remove_file(&path).unwrap();
    symlink(&alias, &path).unwrap();
    assert!(admit_sidecar(&path).unwrap_err().contains("unsafe"));
    assert!(admit_file_metadata(&path, &detached, true).is_err());
    drop(replacement);
    drop(file);
}

#[test]
fn opencode_two_admission_does_not_update_candidates_and_reads_mixed_codecs() {
    let conn = Connection::open_in_memory().unwrap();
    schema(&conn).unwrap();
    let candidate = codec_candidate();
    let legacy = encode(&candidate).unwrap();
    conn.execute("INSERT INTO oc_rows VALUES(1,0,?1)", [&legacy])
        .unwrap();
    conn.execute("UPDATE oc_meta SET version=2", []).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER forbid_admission_candidate_update BEFORE UPDATE ON oc_rows
         BEGIN SELECT RAISE(ABORT,'admission must not recode retained facts'); END;",
    )
    .unwrap();
    let before = conn.total_changes();
    schema(&conn).unwrap();
    assert_eq!(
        conn.total_changes() - before,
        1,
        "only the capability marker changes"
    );
    assert_eq!(
        conn.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row("SELECT candidate FROM oc_rows", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        legacy
    );
    let mut fresh = codec_candidate();
    fresh.message.dedup_key = Some("fresh-row".into());
    let expected_fresh = encode(&fresh).unwrap();
    let source = 1i64;
    SourceWriter {
        conn: &conn,
        source: &source,
        ordinal: 1,
    }
    .push(fresh)
    .unwrap();
    let mut statement = conn
        .prepare("SELECT candidate FROM oc_rows ORDER BY ordinal")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    let first = rows.next().unwrap().unwrap();
    assert_eq!(
        encode(&decode_candidate_row(first, 0).unwrap()).unwrap(),
        legacy
    );
    let second = rows.next().unwrap().unwrap();
    assert_eq!(
        encode(&decode_candidate_row(second, 0).unwrap()).unwrap(),
        expected_fresh
    );
    assert!(second
        .get_ref(0)
        .unwrap()
        .as_blob()
        .unwrap()
        .starts_with(CANDIDATE_FRAME));
    drop(rows);
    drop(statement);
    conn.pragma_update(None, "query_only", true).unwrap();
    let before = conn.total_changes();
    schema(&conn).unwrap();
    assert_eq!(
        conn.total_changes(),
        before,
        "warm mixed-codec admission remains read-only"
    );
}

#[test]
fn explicit_candidate_compaction_leaves_absent_schema_absent_and_rejects_unknown_or_partial() {
    let conn = Connection::open_in_memory().unwrap();
    let before = conn.total_changes();
    compact_opencode_candidates(&conn).unwrap();
    assert_eq!(conn.total_changes(), before);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute_batch("CREATE TABLE oc_meta(version INTEGER); INSERT INTO oc_meta VALUES(2);")
        .unwrap();
    assert!(compact_opencode_candidates(&conn)
        .unwrap_err()
        .contains("incomplete"));
    assert_eq!(
        conn.query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    let unsupported = Connection::open_in_memory().unwrap();
    schema(&unsupported).unwrap();
    unsupported
        .execute("UPDATE oc_meta SET version=4", [])
        .unwrap();
    assert!(compact_opencode_candidates(&unsupported)
        .unwrap_err()
        .contains("unsupported"));
    assert_eq!(
        unsupported
            .query_row("SELECT version FROM oc_meta", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
}
