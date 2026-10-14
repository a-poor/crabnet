//! A public-API smoke client, also copied into an independent package consumer.
//! Run with a fresh data-directory path; existing directories are refused.

use crabnet::{Database, Error, OpenMode, OpenOptions, Parameters, Value, compile_read};
use std::{error::Error as StdError, path::PathBuf};

fn main() -> Result<(), Box<dyn StdError>> {
    let directory = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("supply a fresh data directory")?,
    );
    std::fs::create_dir(&directory)?;
    let path = directory.join("graph.sqlite");
    let backup = directory.join("backup.sqlite");
    let mut graph = Database::open(&path)?;
    graph.create_unique_constraint("Person", "id")?;
    let parameters = Parameters::from([
        ("id".into(), Value::from(1)),
        ("name".into(), Value::from("Ada")),
        ("precise".into(), Value::from(9_007_199_254_740_993_i64)),
    ]);
    let created = graph.execute_with(
        "CREATE (p:Person {id:$id, name:$name, precise:$precise}) RETURN p",
        &parameters,
    )?;
    assert_eq!(created.summary.nodes_created, 1);
    let original = created.rows[0][0].as_node().unwrap().clone();
    assert_eq!(
        original.properties["precise"].as_i64(),
        Some(9_007_199_254_740_993)
    );

    {
        let mut transaction = graph.transaction()?;
        transaction.execute("CREATE (:Person {id:2, name:'Grace'})")?;
        transaction.rollback()?;
    }
    assert_eq!(
        graph.execute("MATCH (p:Person) RETURN count(p)")?.rows[0][0].as_i64(),
        Some(1)
    );
    {
        let mut transaction = graph.transaction()?;
        transaction.execute("CREATE (:Person {id:2, name:'Grace'})")?;
        transaction.commit()?;
    }
    assert!(matches!(
        graph.execute("CREATE (:Person {id:1})"),
        Err(Error::Constraint(_))
    ));
    let read = compile_read("MATCH (p:Person {id:$id}) RETURN p.name")?;
    assert_eq!(
        graph.execute_compiled_with(&read, &parameters)?.rows[0][0].as_str(),
        Some("Ada")
    );
    let rebound = Parameters::from([("id".into(), Value::from(2))]);
    assert_eq!(
        graph.execute_compiled_with(&read, &rebound)?.rows[0][0].as_str(),
        Some("Grace")
    );
    {
        let mut prepared = graph.prepare_read("MATCH (p:Person) RETURN p.id ORDER BY p.id")?;
        let rows = prepared.cursor()?.collect::<crabnet::Result<Vec<_>>>()?;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].as_i64(), Some(1));
        assert_eq!(rows[1][0].as_i64(), Some(2));
    }
    graph.execute("MATCH (a:Person {id:1}), (b:Person {id:2}) CREATE (a)-[:KNOWS]->(b)")?;
    graph.backup_to(&backup)?;
    graph.execute("CREATE (:Person {id:3})")?;
    graph.restore_from(&backup)?;
    drop(graph);
    let mut reopened = Database::open_with_options(
        &path,
        &OpenOptions {
            mode: OpenMode::ReadOnly,
            ..OpenOptions::default()
        },
    )?;
    let restored = reopened.execute("MATCH (p:Person {id:1}) RETURN p")?;
    assert_eq!(restored.rows[0][0].as_node(), Some(&original));
    assert_eq!(
        reopened.execute("MATCH (p:Person) RETURN count(p)")?.rows[0][0].as_i64(),
        Some(2)
    );
    assert_eq!(
        reopened
            .execute("MATCH ()-[r:KNOWS]->() RETURN count(r)")?
            .rows[0][0]
            .as_i64(),
        Some(1)
    );
    assert_eq!(reopened.unique_constraints()?.len(), 1);
    assert!(matches!(
        reopened.execute("CREATE (:Person {id:4})"),
        Err(Error::ReadOnly)
    ));
    println!(
        "package consumer passed: typed values, constraints, transactions, compiled reads, cursor, backup/restore, identity and read-only reopen"
    );
    Ok(())
}
