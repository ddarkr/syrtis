use super::*;

const OC_TABLES: [&str; 6] = [
    "oc_meta",
    "oc_sources",
    "oc_generations",
    "oc_rows",
    "oc_messages",
    "oc_aliases",
];

#[cfg(test)]
thread_local! {
    pub(super) static GROUP_PROBE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

fn group(conn: &Connection, names: &[&str]) -> Result<bool, String> {
    let placeholders = vec!["?"; names.len()].join(",");
    let count: usize = sql(conn.query_row(
        &format!(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ({placeholders})"
        ),
        rusqlite::params_from_iter(names),
        |r| {
            let count = r.get(0)?;
            #[cfg(test)]
            {
                let hook = GROUP_PROBE_HOOK.with(|slot| slot.borrow_mut().take());
                if let Some(hook) = hook {
                    hook();
                }
            }
            Ok(count)
        },
    ))?;
    if count != 0 && count != names.len() {
        return Err("usage store managed schema is incomplete; existing data preserved".into());
    }
    Ok(count != 0)
}

// Maintenance only: no transaction, migration, or main-database writes. Keep
// payload/checksum validation in the durable validator.
pub(super) fn validate_readonly(conn: &Connection) -> Result<(), String> {
    let app: i64 = sql(conn.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let outer: i64 = sql(conn.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if app != APPLICATION_ID || !matches!(outer, 1 | 2) {
        return Err("usage store schema is unsupported; existing data preserved".into());
    }
    let reference = if outer == 1 { "BLOB" } else { "INTEGER" };
    let oc = group(conn, &OC_TABLES)?;
    let sq = group(
        conn,
        &[
            "sq_meta",
            "sq_scopes",
            "sq_scope_generations",
            "sq_scope_sources",
        ],
    )?;
    let decisions = group(conn, &["sq_indexes", "sq_decisions"])?;
    if decisions && !sq {
        return Err("usage store decision schema is incomplete".into());
    }
    if oc {
        columns(conn, "oc_meta", "version:INTEGER")?;
        let valid: bool = sql(conn.query_row(
            "SELECT count(*)=1 AND min(version) BETWEEN ?1 AND ?2 FROM oc_meta",
            [outer, if outer == 2 { 3 } else { outer }],
            |r| r.get(0),
        ))?;
        if !valid {
            return Err("OpenCode usage store schema unsupported".into());
        }
        for (table, spec) in [
            ("oc_sources", "path:BLOB fingerprint:BLOB kind:INTEGER generation:REF"),
            ("oc_generations", "generation:REF path:BLOB fingerprint:BLOB kind:INTEGER"),
            ("oc_rows", "source:REF ordinal:INTEGER candidate:BLOB"),
            ("oc_messages", "source:REF ordinal:INTEGER fingerprint:BLOB embedded_id:TEXT v1_fallback:INTEGER workspace_conflict:INTEGER message:BLOB payload:BLOB timestamp:INTEGER provider:INTEGER primary_key:TEXT"),
            ("oc_aliases", "source:REF ordinal:INTEGER position:INTEGER key:TEXT"),
        ] {
            columns(conn, table, &spec.replace("REF", reference))?;
        }
        if outer == 2 {
            columns(conn, "oc_generations", "descriptor:BLOB")?;
        }
        let descriptor_column = if outer == 1 {
            "generation"
        } else {
            "descriptor"
        };
        let mut statement = sql(conn.prepare(&format!(
            "SELECT {descriptor_column},path,fingerprint,kind FROM oc_generations"
        )))?;
        let mut rows = sql(statement.query([]))?;
        while let Some(row) = sql(rows.next())? {
            validate_descriptor(
                &sql(row.get::<_, Vec<u8>>(0))?,
                &sql(row.get::<_, Vec<u8>>(1))?,
                &sql(row.get::<_, Vec<u8>>(2))?,
                sql(row.get(3))?,
            )?;
        }
        for query in [
            "SELECT EXISTS(SELECT 1 FROM oc_sources s LEFT JOIN oc_generations g ON g.generation=s.generation WHERE g.generation IS NULL OR g.path!=s.path OR g.kind!=s.kind OR g.fingerprint!=s.fingerprint)",
            "SELECT EXISTS(SELECT 1 FROM oc_rows r LEFT JOIN oc_generations g ON g.generation=r.source WHERE g.generation IS NULL)",
            "SELECT EXISTS(SELECT 1 FROM oc_messages m LEFT JOIN oc_rows r ON r.source=m.source AND r.ordinal=m.ordinal WHERE r.source IS NULL)",
            "SELECT EXISTS(SELECT 1 FROM oc_aliases a LEFT JOIN oc_messages m ON m.source=a.source AND m.ordinal=a.ordinal WHERE m.source IS NULL)",
        ] {
            reject_reference(conn, query)?;
        }
    }
    if sq {
        columns(conn, "sq_meta", "key:TEXT value:INTEGER")?;
        // Later scope versions change only derived indexes, not fact references.
        let versions = if outer == 1 { [2, 2] } else { [3, 5] };
        let valid: bool = sql(conn.query_row(
            "SELECT count(*)=1 AND min(value) BETWEEN ?1 AND ?2 FROM sq_meta WHERE key='schema_version'",
            versions,
            |r| r.get(0),
        ))?;
        if !valid {
            return Err("usage scope schema is unsupported".into());
        }
        for (table, spec) in [
            ("sq_scopes", "id:INTEGER scope_key:BLOB current_generation:INTEGER"),
            ("sq_scope_generations", "id:INTEGER scope_id:INTEGER complete:INTEGER selection:INTEGER"),
            ("sq_scope_sources", "scope_generation:INTEGER kind:TEXT lane:TEXT source_order:INTEGER path:BLOB cache_source_id:INTEGER cache_generation:INTEGER generation:REF tag:INTEGER headless:INTEGER workspace:BLOB workspace_label:BLOB phase:INTEGER"),
        ] {
            columns(conn, table, &spec.replace("REF", reference))?;
        }
        reject_reference(conn, "SELECT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN sq_scope_generations g ON g.id=s.scope_generation WHERE g.id IS NULL)")?;
        if oc {
            reject_reference(conn, "SELECT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN oc_generations g ON g.generation=s.generation WHERE s.kind IN ('opencode-sqlite','opencode-json') AND (g.generation IS NULL OR g.path!=s.path OR g.kind!=(s.kind='opencode-sqlite')))")?;
        } else {
            reject_reference(conn, "SELECT EXISTS(SELECT 1 FROM sq_scope_sources WHERE kind IN ('opencode-sqlite','opencode-json'))")?;
        }
    }
    if decisions {
        columns(
            conn,
            "sq_indexes",
            "scope_generation:INTEGER version:INTEGER",
        )?;
        let shared_decisions: bool = sql(conn.query_row(
            "SELECT value=5 FROM sq_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        ))?;
        if shared_decisions {
            columns(conn, "sq_indexes", "decision_generation:INTEGER")?;
        }
        // QueryState upgrades this known optional historical column on write.
        columns(conn, "sq_decisions", &"scope_generation:INTEGER rank:INTEGER lane:TEXT timestamp:INTEGER cache_source_id:INTEGER cache_generation:INTEGER cache_ordinal:INTEGER cache_format?:INTEGER oc_source:REF oc_ordinal:INTEGER retained:INTEGER global_storage:INTEGER workspace:BLOB workspace_label:BLOB model:TEXT headless:INTEGER".replace("REF", reference))?;
        if oc {
            reject_reference(conn, "SELECT EXISTS(SELECT 1 FROM sq_decisions d LEFT JOIN oc_messages m ON m.source=d.oc_source AND m.ordinal=d.oc_ordinal WHERE d.oc_source IS NOT NULL AND m.source IS NULL)")?;
        } else {
            reject_reference(
                conn,
                "SELECT EXISTS(SELECT 1 FROM sq_decisions WHERE oc_source IS NOT NULL)",
            )?;
        }
    }
    if group(conn, &["sq_durable_origins"])? {
        columns(
            conn,
            "sq_durable_origins",
            "scope_key:BLOB namespace:TEXT path:BLOB ownership:BLOB",
        )?;
    }
    let mc = group(
        conn,
        &[
            "mc_meta",
            "mc_sources",
            "mc_versions",
            "mc_rows",
            "mc_codex_turn_ids",
            "mc_migrations",
            "mc_migration_versions",
            "mc_namespaces",
        ],
    )?;
    let storage = group(conn, &["mc_generation_storage"])?;
    if storage && !mc {
        return Err("source message cache schema is incomplete".into());
    }
    if mc {
        for (table, spec) in [
            ("mc_meta", "key:TEXT value:INTEGER"),
            ("mc_sources", "id:INTEGER namespace:TEXT scope:BLOB path:BLOB active:INTEGER generation:INTEGER"),
            ("mc_versions", "source_id:INTEGER generation:INTEGER parser_version:INTEGER format_version:INTEGER fingerprint:BLOB resume:BLOB pi_resume?:BLOB message_count:INTEGER retained_count:INTEGER fallback_count:INTEGER retained_digest:BLOB cacheable:INTEGER codex_turn_count:INTEGER codex_turn_digest:BLOB"),
            ("mc_rows", "source_id:INTEGER generation:INTEGER ordinal:INTEGER dedup_key:TEXT retained:INTEGER fallback:INTEGER timestamp:INTEGER message:BLOB digest:BLOB session_id:TEXT model_id:TEXT client:TEXT provider_id:TEXT workspace:BLOB workspace_label:BLOB"),
            ("mc_codex_turn_ids", "source_id:INTEGER generation:INTEGER turn_id:TEXT"),
            ("mc_migrations", "path:BLOB digest:BLOB source_count:INTEGER"),
            ("mc_migration_versions", "path:BLOB digest:BLOB source_id:INTEGER generation:INTEGER"),
            ("mc_namespaces", "namespace:TEXT signature:BLOB"),
        ] {
            columns(conn, table, spec)?;
        }
        let valid: bool = sql(conn.query_row(
            "SELECT count(*)=1 AND min(value)=?1 FROM mc_meta WHERE key='schema_version'",
            [if storage { 2 } else { 1 }],
            |r| r.get(0),
        ))?;
        if !valid {
            return Err("source message cache schema is incomplete or unsupported".into());
        }
        if storage {
            columns(conn, "mc_generation_storage", "source_id:INTEGER generation:INTEGER row_generation:INTEGER row_count:INTEGER fallback_timestamp:INTEGER format_version:INTEGER turn_generation:INTEGER reference_digest:BLOB")?;
            columns(conn, "mc_codex_turn_ids", "born_generation:INTEGER")?;
        }
        crate::message_cache::install_logical_views_on(conn)?;
    }
    Ok(())
}

fn columns(conn: &Connection, table: &str, spec: &str) -> Result<(), String> {
    let mut statement = sql(conn.prepare(&format!("PRAGMA main.table_info({table})")))?;
    let actual =
        sql(statement.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?))))?
            .collect::<rusqlite::Result<Vec<_>>>();
    let actual = sql(actual)?;
    for required in spec.split_whitespace() {
        let (name, kind) = required.split_once(':').unwrap();
        let optional = name.ends_with('?');
        let name = name.trim_end_matches('?');
        if optional && !actual.iter().any(|(column, _)| column == name) {
            continue;
        }
        if !actual
            .iter()
            .any(|(column, declared)| column == name && declared.eq_ignore_ascii_case(kind))
        {
            return Err(format!(
                "usage store managed column {table}.{name} is missing or unsupported"
            ));
        }
    }
    Ok(())
}

fn validate_descriptor(
    descriptor: &[u8],
    path: &[u8],
    fingerprint: &[u8],
    kind: i64,
) -> Result<(), String> {
    if !matches!(kind, 0 | 1) || encode(&(path, fingerprint, kind == 1))? != descriptor {
        return Err("OpenCode generation descriptor is invalid".into());
    }
    Ok(())
}

fn reject_reference(conn: &Connection, query: &str) -> Result<(), String> {
    if sql(conn.query_row(query, [], |r| r.get::<_, bool>(0)))? {
        return Err("usage store managed reference is dangling".into());
    }
    Ok(())
}

pub(super) fn admit(conn: &Connection) -> Result<(), String> {
    let app: i64 = sql(conn.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(conn.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if app == APPLICATION_ID && version == SCHEMA_VERSION {
        return Ok(());
    }
    // Must precede BEGIN: SQLite cannot change safety level inside a transaction.
    sql(conn.pragma_update(None, "synchronous", "FULL"))?;
    let tx = sql(rusqlite::Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    ))?;
    let app: i64 = sql(tx.pragma_query_value(None, "application_id", |r| r.get(0)))?;
    let version: i64 = sql(tx.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    if app == APPLICATION_ID && version == SCHEMA_VERSION {
        return sql(tx.commit());
    }
    if app == APPLICATION_ID && version == 1 {
        migrate(&tx)?;
    } else {
        let count: i64 = sql(tx.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        ))?;
        if app != 0 || version != 0 || count != 0 {
            return Err("usage store schema is unsupported; existing data preserved".into());
        }
        sql(tx.pragma_update(None, "application_id", APPLICATION_ID))?;
    }
    sql(tx.pragma_update(None, "user_version", SCHEMA_VERSION))?;
    sql(tx.commit())
}

pub(super) fn ensure_opencode(conn: &Connection) -> Result<(), String> {
    if group(conn, &OC_TABLES)? {
        let version: i64 = sql(conn.query_row("SELECT version FROM oc_meta", [], |r| r.get(0)))?;
        if version == 3 {
            return Ok(());
        }
        if version != 2 {
            return Err("OpenCode usage store schema unsupported".into());
        }
    }
    let tx = sql(rusqlite::Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    ))?;
    // Another cold opener may have published the group after our read-only probe.
    // Re-admit it under the initialization lock instead of repairing partial data.
    if group(&tx, &OC_TABLES)? {
        let version: i64 = sql(tx.query_row("SELECT version FROM oc_meta", [], |r| r.get(0)))?;
        if version == 2 {
            // Both codecs are readable. Admission changes only the capability
            // marker; rewriting retained observations belongs to explicit Optimize.
            sql(tx.execute("UPDATE oc_meta SET version=3 WHERE version=2", []))?;
        } else if version != 3 {
            return Err("OpenCode usage store schema unsupported".into());
        }
        return sql(tx.commit());
    }
    sql(tx.execute_batch("CREATE TABLE IF NOT EXISTS oc_meta(version INTEGER NOT NULL);
        INSERT INTO oc_meta SELECT 3 WHERE NOT EXISTS(SELECT 1 FROM oc_meta);
        CREATE TABLE IF NOT EXISTS oc_sources(path BLOB PRIMARY KEY,fingerprint BLOB NOT NULL,kind INTEGER NOT NULL,generation INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS oc_generations(generation INTEGER PRIMARY KEY AUTOINCREMENT,descriptor BLOB NOT NULL UNIQUE,path BLOB NOT NULL,fingerprint BLOB NOT NULL,kind INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS oc_rows(source INTEGER NOT NULL,ordinal INTEGER NOT NULL,candidate BLOB NOT NULL,PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS oc_messages(source INTEGER NOT NULL,ordinal INTEGER NOT NULL,fingerprint BLOB NOT NULL,embedded_id TEXT,v1_fallback INTEGER NOT NULL,workspace_conflict INTEGER NOT NULL,message BLOB NOT NULL,payload BLOB NOT NULL,timestamp INTEGER NOT NULL,provider INTEGER NOT NULL,primary_key TEXT,PRIMARY KEY(source,ordinal)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS oc_fingerprint ON oc_messages(source,fingerprint,ordinal);
        CREATE INDEX IF NOT EXISTS oc_fingerprint_id ON oc_messages(source,fingerprint,embedded_id,ordinal);
        CREATE TABLE IF NOT EXISTS oc_aliases(source INTEGER NOT NULL,ordinal INTEGER NOT NULL,position INTEGER NOT NULL,key TEXT NOT NULL,PRIMARY KEY(source,ordinal,key)) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS oc_alias_order ON oc_aliases(source,ordinal,position);"))?;
    sql(tx.commit())
}

pub(super) fn compact_opencode_candidates(conn: &Connection) -> Result<(), String> {
    if !group(conn, &OC_TABLES)? {
        return Ok(());
    }
    let tx = sql(rusqlite::Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    ))?;
    if group(&tx, &OC_TABLES)? {
        let supported: bool = sql(tx.query_row(
            "SELECT count(*)=1 AND min(version) IN (2,3) FROM oc_meta",
            [],
            |row| row.get(0),
        ))?;
        if !supported {
            return Err("OpenCode usage store schema unsupported".into());
        }
        compact_candidates(&tx)?;
    }
    sql(tx.commit())
}

// Rewrite only the original observation codec. Selection projections and aliases
// remain intact; original float bits and pre-collapse multiplicity are facts.
// Split source/ordinal seeks avoid traversing the already converted prefix.
fn compact_candidates(conn: &Connection) -> Result<(), String> {
    let mut next_source =
        sql(conn
            .prepare_cached("SELECT source FROM oc_rows WHERE source>?1 ORDER BY source LIMIT 1"))?;
    let mut next_row = sql(conn.prepare_cached(
        "SELECT ordinal,candidate FROM oc_rows WHERE source=?1 AND ordinal>=?2 ORDER BY ordinal LIMIT 1",
    ))?;
    let mut update =
        sql(conn.prepare_cached("UPDATE oc_rows SET candidate=?3 WHERE source=?1 AND ordinal=?2"))?;
    let mut source: Option<i64> =
        sql(conn.query_row("SELECT min(source) FROM oc_rows", [], |r| r.get(0)))?;
    while let Some(current_source) = source {
        let mut ordinal = i64::MIN;
        loop {
            let found: Option<(i64, Vec<u8>)> = sql(next_row
                .query_row(params![current_source, ordinal], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional())?;
            let Some((current, bytes)) = found else { break };
            let candidate = decode_candidate(&bytes)?;
            let compact = encode_candidate(&candidate)?;
            if compact != bytes {
                sql(update.execute(params![current_source, current, compact]))?;
            }
            let Some(next) = current.checked_add(1) else {
                break;
            };
            ordinal = next;
        }
        source = sql(next_source
            .query_row([current_source], |r| r.get(0))
            .optional())?;
    }
    sql(conn.execute("UPDATE oc_meta SET version=3 WHERE version=2", []))?;
    Ok(())
}

// Reuse the admitted table definition so optional historical columns and every
// index survive unchanged. Only the one physical reference column changes.
fn rewrite(conn: &Connection, name: &str, column: &str, gate: &str) -> Result<(), String> {
    let definition: String = sql(conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    ))?;
    let mut stmt = sql(conn.prepare("SELECT sql FROM sqlite_master WHERE type='index' AND tbl_name=?1 AND sql IS NOT NULL ORDER BY name"))?;
    let indexes = sql(stmt.query_map([name], |r| r.get::<_, String>(0)))?
        .collect::<rusqlite::Result<Vec<_>>>();
    let indexes = sql(indexes)?;
    drop(stmt);
    let mut stmt = sql(conn.prepare(&format!("PRAGMA table_info({name})")))?;
    let columns =
        sql(stmt.query_map([], |r| r.get::<_, String>(1)))?.collect::<rusqlite::Result<Vec<_>>>();
    let columns = sql(columns)?;
    drop(stmt);
    if !columns.iter().any(|c| c == column) {
        return Err("usage store reference column is missing".into());
    }
    let invalid: bool = sql(conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {name} t WHERE ({gate}) AND t.{column} IS NOT NULL AND NOT EXISTS(SELECT 1 FROM oc_generation_map m WHERE m.descriptor=t.{column}))"), [], |r| r.get(0)))?;
    if invalid {
        return Err("usage store generation mapping is dangling; existing data preserved".into());
    }
    let staged = format!("normalized_{name}");
    let typed = definition.replace(&format!("{column} BLOB"), &format!("{column} INTEGER"));
    if typed == definition {
        return Err("usage store legacy reference declaration is unsupported".into());
    }
    sql(conn.execute_batch(&typed.replacen(name, &staged, 1)))?;
    let projection = columns.iter().map(|c| if c == column {
        format!("CASE WHEN ({gate}) THEN (SELECT generation FROM oc_generation_map WHERE descriptor=t.{column}) ELSE t.{column} END")
    } else { format!("t.\"{c}\"") }).collect::<Vec<_>>().join(",");
    sql(conn.execute_batch(&format!("INSERT INTO {staged} SELECT {projection} FROM {name} t; DROP TABLE {name}; ALTER TABLE {staged} RENAME TO {name};")))?;
    for index in indexes {
        sql(conn.execute_batch(&index))?;
    }
    Ok(())
}

fn migrate(conn: &Connection) -> Result<(), String> {
    let oc = group(conn, &OC_TABLES)?;
    let sq = group(
        conn,
        &[
            "sq_meta",
            "sq_scopes",
            "sq_scope_generations",
            "sq_scope_sources",
        ],
    )?;
    let decisions = group(conn, &["sq_indexes", "sq_decisions"])?;
    if decisions && !sq {
        return Err("usage store decision schema is incomplete".into());
    }
    if sq {
        let version: i64 = sql(conn.query_row(
            "SELECT value FROM sq_meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        ))?;
        if version != 2 {
            return Err("usage scope schema is unsupported; existing data preserved".into());
        }
    }
    if oc {
        for query in [
            "SELECT path,fingerprint,kind,generation FROM oc_sources LIMIT 0",
            "SELECT generation,path,fingerprint,kind FROM oc_generations LIMIT 0",
            "SELECT source,ordinal,candidate FROM oc_rows LIMIT 0",
            "SELECT source,ordinal,fingerprint,embedded_id,v1_fallback,workspace_conflict,message,payload,timestamp,provider,primary_key FROM oc_messages LIMIT 0",
            "SELECT source,ordinal,position,key FROM oc_aliases LIMIT 0",
        ] { sql(conn.prepare(query))?; }
        for query in [
            "SELECT EXISTS(SELECT 1 FROM oc_messages m LEFT JOIN oc_rows r ON r.source=m.source AND r.ordinal=m.ordinal WHERE r.source IS NULL)",
            "SELECT EXISTS(SELECT 1 FROM oc_aliases a LEFT JOIN oc_messages m ON m.source=a.source AND m.ordinal=a.ordinal WHERE m.source IS NULL)",
        ] {
            if sql(conn.query_row(query, [], |r| r.get::<_, bool>(0)))? {
                return Err("OpenCode legacy fact reference is dangling".into());
            }
        }
    }
    if sq {
        let missing: bool = sql(conn.query_row("SELECT EXISTS(SELECT 1 FROM sq_scope_sources WHERE kind IN ('opencode-sqlite','opencode-json') AND generation IS NULL)", [], |r| r.get(0)))?;
        if missing {
            return Err("OpenCode scope reference is missing".into());
        }
        if oc {
            let invalid: bool = sql(conn.query_row("SELECT EXISTS(SELECT 1 FROM sq_scope_sources s LEFT JOIN oc_generations g ON g.generation=s.generation WHERE s.kind IN ('opencode-sqlite','opencode-json') AND (g.generation IS NULL OR g.path!=s.path OR g.kind!=(s.kind='opencode-sqlite')))", [], |r| r.get(0)))?;
            if invalid {
                return Err("OpenCode scope descriptor is invalid".into());
            }
        }
    }
    if decisions && oc {
        let invalid: bool = sql(conn.query_row("SELECT EXISTS(SELECT 1 FROM sq_decisions d LEFT JOIN oc_messages m ON m.source=d.oc_source AND m.ordinal=d.oc_ordinal WHERE d.oc_source IS NOT NULL AND m.source IS NULL)", [], |r| r.get(0)))?;
        if invalid {
            return Err("OpenCode decision reference is dangling".into());
        }
    }
    sql(conn.execute_batch("CREATE TEMP TABLE oc_generation_map(descriptor BLOB PRIMARY KEY,generation INTEGER NOT NULL UNIQUE) WITHOUT ROWID;"))?;
    if oc {
        let version: i64 = sql(conn.query_row("SELECT version FROM oc_meta", [], |r| r.get(0)))?;
        let count: i64 = sql(conn.query_row("SELECT count(*) FROM oc_meta", [], |r| r.get(0)))?;
        if version != 1 || count != 1 {
            return Err("OpenCode legacy schema unsupported".into());
        }
        sql(conn.execute_batch("CREATE TABLE normalized_oc_generations(generation INTEGER PRIMARY KEY AUTOINCREMENT,descriptor BLOB NOT NULL UNIQUE,path BLOB NOT NULL,fingerprint BLOB NOT NULL,kind INTEGER NOT NULL);"))?;
        let mut stmt = sql(conn.prepare(
            "SELECT generation,path,fingerprint,kind FROM oc_generations ORDER BY generation",
        ))?;
        let mut rows = sql(stmt.query([]))?;
        while let Some(row) = sql(rows.next())? {
            let descriptor: Vec<u8> = sql(row.get(0))?;
            let path: Vec<u8> = sql(row.get(1))?;
            let fingerprint: Vec<u8> = sql(row.get(2))?;
            let kind: i64 = sql(row.get(3))?;
            validate_descriptor(&descriptor, &path, &fingerprint, kind)?;
            sql(conn.execute("INSERT INTO normalized_oc_generations(descriptor,path,fingerprint,kind) VALUES(?1,?2,?3,?4)", params![descriptor,path,fingerprint,kind]))?;
            sql(conn.execute(
                "INSERT INTO oc_generation_map VALUES(?1,?2)",
                params![descriptor, conn.last_insert_rowid()],
            ))?;
        }
        drop(rows);
        drop(stmt);
        let invalid: bool = sql(conn.query_row("SELECT EXISTS(SELECT 1 FROM oc_sources s LEFT JOIN oc_generations g ON g.generation=s.generation WHERE g.generation IS NULL OR g.path!=s.path OR g.kind!=s.kind OR g.fingerprint!=s.fingerprint)", [], |r| r.get(0)))?;
        if invalid {
            return Err("OpenCode current source descriptor is invalid".into());
        }
        for (name, column) in [
            ("oc_sources", "generation"),
            ("oc_rows", "source"),
            ("oc_messages", "source"),
            ("oc_aliases", "source"),
        ] {
            rewrite(conn, name, column, "1")?;
        }
        sql(conn.execute_batch("DROP TABLE oc_generations; ALTER TABLE normalized_oc_generations RENAME TO oc_generations; UPDATE oc_meta SET version=2;"))?;
    }
    if sq {
        rewrite(
            conn,
            "sq_scope_sources",
            "generation",
            "t.kind IN ('opencode-sqlite','opencode-json')",
        )?;
    }
    if decisions {
        rewrite(conn, "sq_decisions", "oc_source", "1")?;
    }
    for (present, name, column) in [
        (oc, "oc_sources", "generation"),
        (oc, "oc_rows", "source"),
        (oc, "oc_messages", "source"),
        (oc, "oc_aliases", "source"),
        (sq, "sq_scope_sources", "generation"),
        (decisions, "sq_decisions", "oc_source"),
    ] {
        if present {
            let invalid: bool = sql(conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {name} WHERE {column} IS NOT NULL AND typeof({column})!='integer')"), [], |r| r.get(0)))?;
            if invalid {
                return Err("usage store compact generation reference is invalid".into());
            }
        }
    }
    if sq {
        sql(conn.execute("UPDATE sq_meta SET value=3 WHERE key='schema_version'", []))?;
    }
    crate::store_query::compact_decision_metadata(conn)?;
    #[cfg(test)]
    MIGRATION_FAILURE.with(|fail| {
        if fail.get() {
            Err("injected usage schema migration failure".to_string())
        } else {
            Ok(())
        }
    })?;
    sql(conn.execute_batch("DROP TABLE temp.oc_generation_map;"))?;
    Ok(())
}

#[cfg(test)]
thread_local! { pub(super) static MIGRATION_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
