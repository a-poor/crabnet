//! Changing-key commit and parent reads derived from Git Graph's public-API
//! reproducer. Explicit fixture oracles validate both execution strategies.
use crabnet::{
    CompiledRead, Database, Parameters, QueryOptions, QueryResult, Result, compile_read,
};
use serde_json::json;

pub const COMMIT: &str = "MATCH (c:Commit {oid:$oid}) RETURN c";
pub const PARENTS: &str =
    "MATCH (:Commit {oid:$oid})-[r:PARENT]->(p) RETURN r.position, p.oid, labels(p)";
pub const OIDS: [&str; 4] = ["root", "left", "right", "merge"];

pub struct Fixture {
    pub db: Database,
    pub reads: [CompiledRead; 2],
    pub options: QueryOptions,
    parameters: [Parameters; 4],
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let mut db = Database::in_memory()?;
        db.create_unique_constraint("Commit", "oid")?;
        db.execute("CREATE (r:Commit {oid:'root'}), (l:Commit {oid:'left'}), (b:Commit {oid:'right'}), (m:Commit {oid:'merge'}), (l)-[:PARENT {position:0}]->(r), (b)-[:PARENT {position:0}]->(r), (m)-[:PARENT {position:1}]->(l), (m)-[:PARENT {position:0}]->(b)")?;
        Ok(Self {
            db,
            reads: [compile_read(COMMIT)?, compile_read(PARENTS)?],
            options: QueryOptions {
                max_result_rows: Some(2),
                max_result_bytes: Some(4096),
                max_materialized_rows: Some(32),
                max_materialized_bytes: Some(16384),
                ..QueryOptions::default()
            },
            parameters: OIDS.map(|oid| Parameters::from([("oid".into(), json!(oid))])),
        })
    }

    pub fn read(&mut self, compiled: bool, index: usize) -> Result<[QueryResult; 2]> {
        let parameters = &self.parameters[index % OIDS.len()];
        if compiled {
            Ok([
                self.db
                    .execute_compiled_with_options(&self.reads[0], parameters, &self.options)?,
                self.db
                    .execute_compiled_with_options(&self.reads[1], parameters, &self.options)?,
            ])
        } else {
            Ok([
                self.db
                    .execute_with_options(COMMIT, parameters, &self.options)?,
                self.db
                    .execute_with_options(PARENTS, parameters, &self.options)?,
            ])
        }
    }

    pub fn check(index: usize, [commit, parents]: &[QueryResult; 2]) -> usize {
        let oid = OIDS[index % OIDS.len()];
        assert_eq!(commit.columns, ["c"]);
        assert_eq!(commit.rows.len(), 1);
        let node = commit.rows[0][0].as_node().unwrap();
        assert_eq!(node.labels, ["Commit"]);
        assert_eq!(node.properties.get("oid").unwrap().as_str(), Some(oid));
        assert_eq!(node.properties.len(), 1);
        assert_eq!(commit.summary, Default::default());
        assert_eq!(parents.columns, ["r.position", "p.oid", "labels(p)"]);
        assert_eq!(parents.summary, Default::default());
        let expected: &[&str] = match oid {
            "root" => &[],
            "left" | "right" => &["root"],
            "merge" => &["right", "left"],
            _ => unreachable!(),
        };
        let mut actual = parents
            .rows
            .iter()
            .map(|row| {
                assert_eq!(row.len(), 3);
                assert_eq!(row[2].as_list().unwrap().len(), 1);
                assert_eq!(row[2].as_list().unwrap()[0].as_str(), Some("Commit"));
                (row[0].as_i64().unwrap(), row[1].as_str().unwrap())
            })
            .collect::<Vec<_>>();
        actual.sort_unstable(); // Preserve duplicate positions and rows.
        assert_eq!(
            actual,
            expected
                .iter()
                .enumerate()
                .map(|(i, oid)| (i as i64, *oid))
                .collect::<Vec<_>>()
        );
        commit.rows.len() + parents.rows.len()
    }
}
