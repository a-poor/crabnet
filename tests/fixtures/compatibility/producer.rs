// Copied into each historical checkout by rebuild.py. The marked statements
// are inserted only when that writer exposes the corresponding public API.
use std::{env, fs, path::PathBuf};

use crabnet::{Database, Parameters};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<String> = env::args().collect();
    let version: i64 = arguments[1].parse()?;
    let fixture = PathBuf::from(&arguments[2]);
    let expected = PathBuf::from(&arguments[3]);
    let working = fixture.with_extension("working.sqlite");
    assert!(!fixture.exists() && !working.exists() && !expected.exists());
    let mut database = Database::open(&working)?;
    let parameters = Parameters::from([
        ("precise".into(), json!(1.2345678901234567_f64)),
        ("text".into(), json!("O'Reilly\n雪")),
    ]);
    database.execute_with(
        "CREATE (a:Person {key:'a', number:1, explicit:null,
            nested:{items:[1,2.0,null,{flag:true}]}, precise:$precise,
            big:9007199254740993, text:$text}),
         (b:Person {key:'b', number:2.0, active:false}),
         (bare {key:'bare', empty:[]}),
         (:Person {key:null}), (:Person {role:'missing'}),
         (a)-[:LINK {weight:2, notes:['same',null]}]->(b),
         (a)-[:LINK {weight:2, notes:['same',null]}]->(b),
         (b)-[:SELF {active:false}]->(b), (b)-[:BACK]->(a)",
        &parameters,
    )?;
    // ADD_LABEL
    // ADD_CONSTRAINTS
    // MAKE_BACKUP
    drop(database);
    if !fixture.exists() {
        // Writers predating backup_to: all handles are closed and the DELETE
        // journal is gone before copying the standalone database.
        assert!(!PathBuf::from(format!("{}-journal", working.display())).exists());
        fs::copy(&working, &fixture)?;
    }
    // Prove the original writer can reopen its own rollback copy/backup.
    let mut reopened = Database::open(&fixture)?;
    assert_eq!(
        reopened.execute("MATCH (n) RETURN count(n)")?.rows[0][0].as_i64(),
        Some(5)
    );
    assert_eq!(
        reopened.execute("MATCH ()-[r]->() RETURN count(r)")?.rows[0][0].as_i64(),
        Some(4)
    );
    drop(reopened);

    // Capture an oracle from the old writer's persisted rows, independently
    // of the new reader's migration, hydration, or schema constants.
    let connection = Connection::open_with_flags(&fixture, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let schema_version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    assert_eq!(schema_version, version);
    let node_sql = if version == 1 {
        "SELECT id, CASE WHEN label IS NULL THEN '[]' ELSE json_array(label) END,
         props FROM nodes ORDER BY id"
    } else {
        "SELECT id, labels, props FROM nodes ORDER BY id"
    };
    let nodes: Vec<Value> = connection
        .prepare(node_sql)?
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "labels": serde_json::from_str::<Value>(&row.get::<_, String>(1)?).unwrap(),
                "properties": serde_json::from_str::<Value>(&row.get::<_, String>(2)?).unwrap(),
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let relationships: Vec<Value> = connection
        .prepare("SELECT id, edge_type, from_node, to_node, props FROM edges ORDER BY id")?
        .query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "type": row.get::<_, String>(1)?,
                "from": row.get::<_, String>(2)?,
                "to": row.get::<_, String>(3)?,
                "properties": serde_json::from_str::<Value>(&row.get::<_, String>(4)?).unwrap(),
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let constraints: Vec<Value> = if version >= 4 {
        connection
            .prepare(
                "SELECT label, property FROM node_unique_constraints ORDER BY label, property",
            )?
            .query_map([], |row| {
                Ok(json!({"label": row.get::<_, String>(0)?, "property": row.get::<_, String>(1)?}))
            })?
            .collect::<rusqlite::Result<_>>()?
    } else {
        Vec::new()
    };
    let snapshot = json!({
        "schema_version": version, "sqlite_version": rusqlite::version(),
        "nodes": nodes, "relationships": relationships, "constraints": constraints,
    });
    fs::write(expected, serde_json::to_string_pretty(&snapshot)? + "\n")?;
    fs::remove_file(working)?;
    Ok(())
}
