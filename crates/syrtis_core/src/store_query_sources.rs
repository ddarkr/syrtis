use rusqlite::{params, Connection};

const FIELDS: &str = "kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase";
const TYPES: &str = "kind TEXT NOT NULL,lane TEXT NOT NULL,source_order INTEGER NOT NULL,path BLOB NOT NULL,cache_source_id INTEGER,cache_generation INTEGER,generation INTEGER,tag INTEGER NOT NULL,headless INTEGER NOT NULL,workspace BLOB,workspace_label BLOB,phase INTEGER NOT NULL";
const VIEW: &str = "CREATE VIEW sq_scope_sources AS SELECT s.current_generation AS scope_generation,c.kind,c.lane,c.source_order,c.path,c.cache_source_id,c.cache_generation,c.generation,c.tag,c.headless,c.workspace,c.workspace_label,c.phase FROM sq_scope_source_current c JOIN sq_scopes s ON s.id=c.scope_id UNION ALL SELECT scope_generation,kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase FROM sq_scope_source_pins";

fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|error| error.to_string())
}

fn reject(db: &Connection, query: &str) -> Result<(), String> {
    if sql(db.query_row(query, [], |row| row.get::<_, bool>(0)))? {
        return Err("usage scope source registry is invalid; existing data preserved".into());
    }
    Ok(())
}

fn validate_rows(db: &Connection) -> Result<(), String> {
    reject(db, "SELECT EXISTS(SELECT 1 FROM main.sq_scope_sources WHERE typeof(scope_generation)!='integer' OR typeof(kind)!='text' OR typeof(lane)!='text' OR typeof(source_order)!='integer' OR typeof(path)!='blob' OR typeof(cache_source_id) NOT IN ('null','integer') OR typeof(cache_generation) NOT IN ('null','integer') OR typeof(generation) NOT IN ('null','integer') OR typeof(tag)!='integer' OR typeof(headless)!='integer' OR typeof(workspace) NOT IN ('null','blob') OR typeof(workspace_label) NOT IN ('null','blob') OR typeof(phase)!='integer')")?;
    reject(db, "SELECT EXISTS(SELECT 1 FROM main.sq_scope_sources x LEFT JOIN sq_scope_generations g ON g.id=x.scope_generation WHERE g.id IS NULL)")?;
    reject(db, "SELECT EXISTS(SELECT 1 FROM sq_scopes s LEFT JOIN sq_scope_generations g ON g.id=s.current_generation WHERE s.current_generation IS NOT NULL AND (g.id IS NULL OR g.scope_id!=s.id OR g.complete!=1))")
}

pub(crate) fn validate_source_registry(db: &Connection) -> Result<(), String> {
    let definition: String = sql(db.query_row(
        "SELECT sql FROM sqlite_master WHERE type='view' AND name='sq_scope_sources'",
        [],
        |row| row.get(0),
    ))?;
    if definition != VIEW {
        return Err("usage scope source view is unsupported".into());
    }
    for (table, key) in [
        ("sq_scope_source_current", "scope_id"),
        ("sq_scope_source_pins", "scope_generation"),
    ] {
        let physical: bool = sql(db.query_row(
            "SELECT count(*)=1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get(0),
        ))?;
        if !physical {
            return Err("usage scope source physical table is missing".into());
        }
        let expected = format!("{key}:INTEGER kind:TEXT lane:TEXT source_order:INTEGER path:BLOB cache_source_id:INTEGER cache_generation:INTEGER generation:INTEGER tag:INTEGER headless:INTEGER workspace:BLOB workspace_label:BLOB phase:INTEGER");
        let mut statement = sql(db.prepare(&format!("PRAGMA main.table_info({table})")))?;
        let actual = sql(statement.query_map([], |row| {
            Ok(format!(
                "{}:{}",
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?.to_uppercase()
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?
        .join(" ");
        if actual != expected {
            return Err("usage scope source physical schema is unsupported".into());
        }
        let primary = sql(db.prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') WHERE pk>0 ORDER BY pk"
        )))?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
        if primary != [key, "kind", "lane", "source_order"] {
            return Err("usage scope source key is unsupported".into());
        }
    }
    let trigger: String = sql(db.query_row(
        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='sq_scope_source_evict'",
        [],
        |row| row.get(0),
    ))?;
    if trigger != "CREATE TRIGGER sq_scope_source_evict AFTER DELETE ON sq_scopes BEGIN DELETE FROM sq_scope_source_current WHERE scope_id=OLD.id; END" {
        return Err("usage scope source eviction is unsupported".into());
    }
    Ok(())
}

fn registry_version(db: &Connection) -> Result<i64, String> {
    let (count, version): (i64, Option<i64>) = sql(db.query_row(
        "SELECT count(*),min(CASE WHEN typeof(value)='integer' THEN value END) FROM sq_meta WHERE key='schema_version'",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ))?;
    match (count, version) {
        (1, Some(version @ (5 | 6))) => Ok(version),
        _ => Err("usage scope schema is unsupported; existing data preserved".into()),
    }
}

pub(crate) fn ensure_source_registry(db: &Connection) -> Result<(), String> {
    if registry_version(db)? == 6 {
        return validate_source_registry(db);
    }
    let tx = sql(rusqlite::Transaction::new_unchecked(
        db,
        rusqlite::TransactionBehavior::Immediate,
    ))?;
    // Another connection may have completed migration while we acquired the lock.
    let version = registry_version(&tx)?;
    if version == 5 {
        let physical: bool = sql(tx.query_row(
            "SELECT count(*)=1 FROM sqlite_master WHERE type='table' AND name='sq_scope_sources'",
            [],
            |row| row.get(0),
        ))?;
        let mut columns = sql(tx.prepare(
            "SELECT name,upper(type) FROM pragma_table_xinfo('sq_scope_sources') ORDER BY cid",
        ))?;
        let actual = sql(columns.query_map([], |row| {
            Ok(format!(
                "{}:{}",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?
        .join(" ");
        let expected = "scope_generation:INTEGER kind:TEXT lane:TEXT source_order:INTEGER path:BLOB cache_source_id:INTEGER cache_generation:INTEGER generation:INTEGER tag:INTEGER headless:INTEGER workspace:BLOB workspace_label:BLOB phase:INTEGER";
        if !physical || actual != expected {
            return Err("usage scope source schema is unsupported".into());
        }
        drop(columns);
        validate_rows(&tx)?;
        for (table, key) in [
            ("sq_scope_source_current", "scope_id"),
            ("sq_scope_source_pins", "scope_generation"),
        ] {
            sql(tx.execute_batch(&format!("CREATE TABLE {table}({key} INTEGER NOT NULL,{TYPES},PRIMARY KEY({key},kind,lane,source_order)) WITHOUT ROWID;")))?;
        }
        sql(tx.execute_batch(&format!("INSERT INTO sq_scope_source_current SELECT s.id,{FIELDS} FROM sq_scope_sources x JOIN sq_scopes s ON s.current_generation=x.scope_generation; INSERT INTO sq_scope_source_pins SELECT * FROM sq_scope_sources x WHERE NOT EXISTS(SELECT 1 FROM sq_scopes s WHERE s.current_generation=x.scope_generation); DROP TABLE sq_scope_sources; {VIEW}; CREATE TRIGGER sq_scope_source_evict AFTER DELETE ON sq_scopes BEGIN DELETE FROM sq_scope_source_current WHERE scope_id=OLD.id; END;")))?;
        for (table, key, suffix) in [
            ("sq_scope_source_current", "scope_id", "current"),
            ("sq_scope_source_pins", "scope_generation", "pins"),
        ] {
            sql(tx.execute_batch(&format!("CREATE INDEX sq_source_cache_{suffix} ON {table}(cache_source_id,cache_generation); CREATE INDEX sq_source_opencode_{suffix} ON {table}(kind,path,generation) WHERE kind IN ('opencode-sqlite','opencode-json'); CREATE INDEX sq_source_replay_{suffix} ON {table}({key},lane,phase,source_order) WHERE kind IN ('cache','observed','observed-legacy');")))?;
        }
        sql(tx.execute("UPDATE sq_meta SET value=6 WHERE key='schema_version'", []))?;
        reject(&tx, "SELECT EXISTS(SELECT 1 FROM sq_scope_source_current c LEFT JOIN sq_scopes s ON s.id=c.scope_id WHERE s.id IS NULL OR s.current_generation IS NULL)")?;
        reject(&tx, "SELECT EXISTS(SELECT 1 FROM sq_scope_source_pins p JOIN sq_scopes s ON s.current_generation=p.scope_generation)")?;
        validate_rows(&tx)?;
    }
    validate_source_registry(&tx)?;
    sql(tx.commit())
}

// Publication moves staged pins into the owning scope in the same transaction
// that switches current_generation. Readers keep their existing SQLite snapshot.
pub(crate) fn publish_sources_on(db: &Connection, generation: i64) -> Result<(), String> {
    if db.is_autocommit() {
        return Err("source publication requires a transaction".into());
    }
    let scope: i64 = sql(db.query_row(
        "SELECT scope_id FROM sq_scope_generations WHERE id=?1",
        [generation],
        |row| row.get(0),
    ))?;
    // Index reuse compares the old and new source sets after the pointer switch.
    // Keep the old descriptors visible until publication GC retires their header.
    sql(db.execute(&format!("INSERT INTO sq_scope_source_pins SELECT s.current_generation,{FIELDS} FROM sq_scope_source_current c JOIN sq_scopes s ON s.id=c.scope_id WHERE c.scope_id=?1 AND s.current_generation IS NOT NULL"), [scope]))?;
    sql(db.execute("DELETE FROM sq_scope_source_current AS c WHERE scope_id=?1 AND NOT EXISTS(SELECT 1 FROM sq_scope_source_pins n WHERE n.scope_generation=?2 AND n.kind=c.kind AND n.lane=c.lane AND n.source_order=c.source_order)", params![scope, generation]))?;
    sql(db.execute("INSERT INTO sq_scope_source_current SELECT ?1,kind,lane,source_order,path,cache_source_id,cache_generation,generation,tag,headless,workspace,workspace_label,phase FROM sq_scope_source_pins WHERE scope_generation=?2 ON CONFLICT(scope_id,kind,lane,source_order) DO UPDATE SET path=excluded.path,cache_source_id=excluded.cache_source_id,cache_generation=excluded.cache_generation,generation=excluded.generation,tag=excluded.tag,headless=excluded.headless,workspace=excluded.workspace,workspace_label=excluded.workspace_label,phase=excluded.phase WHERE (sq_scope_source_current.path,sq_scope_source_current.cache_source_id,sq_scope_source_current.cache_generation,sq_scope_source_current.generation,sq_scope_source_current.tag,sq_scope_source_current.headless,sq_scope_source_current.workspace,sq_scope_source_current.workspace_label,sq_scope_source_current.phase) IS NOT (excluded.path,excluded.cache_source_id,excluded.cache_generation,excluded.generation,excluded.tag,excluded.headless,excluded.workspace,excluded.workspace_label,excluded.phase)", params![scope, generation]))?;
    sql(db.execute(
        "DELETE FROM sq_scope_source_pins WHERE scope_generation=?1",
        [generation],
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(db: &Connection) -> Vec<Vec<rusqlite::types::Value>> {
        db.prepare(
            "SELECT * FROM sq_scope_sources ORDER BY scope_generation,kind,lane,source_order",
        )
        .unwrap()
        .query_map([], |row| (0..13).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    }

    #[test]
    fn legacy_registry_upgrade_preserves_current_pending_and_epoch_atomically() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(&format!(
            "CREATE TABLE sq_meta(key TEXT PRIMARY KEY,value INTEGER NOT NULL);
             INSERT INTO sq_meta VALUES('schema_version',5),('store_epoch',x'000102030405060708090a0b0c0d0e0f');
             CREATE TABLE sq_scopes(id INTEGER PRIMARY KEY,scope_key BLOB NOT NULL UNIQUE,current_generation INTEGER);
             INSERT INTO sq_scopes VALUES(7,x'01',2);
             CREATE TABLE sq_scope_generations(id INTEGER PRIMARY KEY,scope_id INTEGER NOT NULL,complete INTEGER NOT NULL,selection INTEGER NOT NULL);
             INSERT INTO sq_scope_generations VALUES(1,7,0,0),(2,7,1,0),(3,7,0,0);
             CREATE TABLE sq_scope_sources(scope_generation INTEGER NOT NULL,{TYPES},PRIMARY KEY(scope_generation,kind,lane,source_order)) WITHOUT ROWID;
             INSERT INTO sq_scope_sources VALUES(1,'cache','amp',0,x'01',10,11,NULL,0,1,NULL,x'00',1),
                 (2,'cache','amp',0,x'02',20,21,NULL,1,0,x'00',NULL,0),
                 (3,'cache','amp',0,x'03',30,31,NULL,0,0,x'01',x'02',2);
             CREATE TABLE sq_scope_source_pins(unrelated BLOB);"
        )).unwrap();
        let before = rows(&db);
        assert!(ensure_source_registry(&db).is_err());
        assert_eq!(registry_version(&db).unwrap(), 5);
        assert_eq!(rows(&db), before);
        db.execute_batch("DROP TABLE sq_scope_source_pins").unwrap();
        ensure_source_registry(&db).unwrap();
        assert_eq!(rows(&db), before);
        let epoch: Vec<u8> = db
            .query_row(
                "SELECT value FROM sq_meta WHERE key='store_epoch'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(epoch, (0..16).collect::<Vec<u8>>());
        // A failure after moving descriptors must roll back the pointer and rows.
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        publish_sources_on(&db, 3).unwrap();
        db.execute_batch("UPDATE sq_scopes SET current_generation=3; ROLLBACK")
            .unwrap();
        assert_eq!(rows(&db), before);
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        publish_sources_on(&db, 3).unwrap();
        db.execute_batch("UPDATE sq_scopes SET current_generation=3; UPDATE sq_scope_generations SET complete=1 WHERE id=3; COMMIT").unwrap();
        let after = rows(&db);
        assert_eq!(after, before);
        assert!(publish_sources_on(&db, 3).is_err());
    }
}
