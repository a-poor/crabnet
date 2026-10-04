use std::time::{Duration, Instant};

use crabnet::{CypherValue, Database, Error, Parameters, QueryOptions, QueryResult};
use serde_json::{Value, json};

use super::{AppResult, model::Snapshot};

// Project the key into a scalar slot so the MERGE matcher can probe the
// property index instead of scanning Package nodes for a computed map access.
const NODE_IMPORT: &str = "UNWIND $rows AS row
    WITH row, row.id AS package_id
    MERGE (p:Package {package_id: package_id}) ON CREATE SET p.reviewed = false
    SET p.name = row.name, p.version = row.version, p.workspace = row.workspace,
        p.generation = $generation";

const EDGE_IMPORT: &str = "UNWIND $rows AS row
    WITH row.source AS source_id, row.target AS target_id
    MATCH (source:Package), (target:Package)
    WHERE source.package_id = source_id AND target.package_id = target_id
    MERGE (source)-[r:DEPENDS_ON]->(target) SET r.generation = $generation";

const DEPENDENCIES: &str = "MATCH (:Package {package_id: $id})-[:DEPENDS_ON*1..4]->(p)
    RETURN DISTINCT p.package_id, p.name, p.version ORDER BY p.package_id";
const DEPENDENTS: &str = "MATCH (:Package {package_id: $id})<-[:DEPENDS_ON*1..4]-(p)
    RETURN DISTINCT p.package_id, p.name, p.version ORDER BY p.package_id";

pub fn options() -> QueryOptions {
    QueryOptions {
        deadline: Some(Instant::now() + Duration::from_secs(60)),
        max_materialized_rows: Some(1_000_000),
        max_materialized_bytes: Some(128 * 1024 * 1024),
        max_result_rows: Some(250_000),
        max_result_bytes: Some(64 * 1024 * 1024),
        ..QueryOptions::default()
    }
}

pub fn parameters<const N: usize>(values: [(&str, Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
}

/// An authoritative snapshot replaces this example's Package/DEPENDS_ON graph.
/// The generation marker, upserts, and pruning all share one transaction.
pub fn import(
    database: &mut Database,
    snapshot: &Snapshot,
    batch_size: usize,
    controls: &QueryOptions,
) -> AppResult<Value> {
    snapshot.validate()?;
    if !(1..=1024).contains(&batch_size) {
        return Err("batch size must be between 1 and 1024".into());
    }
    let started = Instant::now();
    let mut transaction = database.transaction()?;
    transaction.create_unique_constraint("Package", "package_id")?;
    transaction.create_unique_constraint("DependencyImport", "key")?;
    let result = transaction.execute_with_options(
        "MERGE (s:DependencyImport {key: 'cargo'}) ON CREATE SET s.generation = 0
         SET s.generation = s.generation + 1 RETURN s.generation",
        &Parameters::new(),
        controls,
    )?;
    let generation = result.rows[0][0]
        .as_i64()
        .ok_or("invalid import generation")?;
    let nodes_started = Instant::now();
    let mut nodes_created = 0;
    for batch in snapshot.packages.chunks(batch_size) {
        let rows: Vec<_> = batch
            .iter()
            .map(|p| {
                json!({
                    "id": p.id, "name": p.name, "version": p.version, "workspace": p.workspace,
                })
            })
            .collect();
        nodes_created += transaction
            .execute_with_options(
                NODE_IMPORT,
                &parameters([("rows", json!(rows)), ("generation", json!(generation))]),
                controls,
            )?
            .summary
            .nodes_created;
    }
    let nodes_ms = nodes_started.elapsed().as_secs_f64() * 1000.0;
    let edges_started = Instant::now();
    let mut edges_created = 0;
    for batch in snapshot.dependencies.chunks(batch_size) {
        let rows: Vec<_> = batch
            .iter()
            .map(|edge| json!({"source": edge.source, "target": edge.target}))
            .collect();
        edges_created += transaction
            .execute_with_options(
                EDGE_IMPORT,
                &parameters([("rows", json!(rows)), ("generation", json!(generation))]),
                controls,
            )?
            .summary
            .relationships_created;
    }
    let edges_ms = edges_started.elapsed().as_secs_f64() * 1000.0;
    let prune_started = Instant::now();
    let values = parameters([("generation", json!(generation))]);
    let edges_deleted = transaction.execute_with_options(
        "MATCH ()-[r:DEPENDS_ON]->() WHERE r.generation IS NULL OR r.generation <> $generation DELETE r",
        &values, controls,
    )?.summary.relationships_deleted;
    let nodes_deleted = transaction.execute_with_options(
        "MATCH (p:Package) WHERE p.generation IS NULL OR p.generation <> $generation DETACH DELETE p",
        &values, controls,
    )?.summary.nodes_deleted;
    let prune_ms = prune_started.elapsed().as_secs_f64() * 1000.0;
    let commit_started = Instant::now();
    transaction.commit()?;
    Ok(
        json!({"generation": generation, "nodes_created": nodes_created, "edges_created": edges_created,
        "nodes_deleted": nodes_deleted, "edges_deleted": edges_deleted,
        "nodes_ms": nodes_ms, "edges_ms": edges_ms, "prune_ms": prune_ms,
        "commit_ms": commit_started.elapsed().as_secs_f64() * 1000.0,
        "total_ms": started.elapsed().as_secs_f64() * 1000.0}),
    )
}

pub fn lookup(database: &mut Database, id: &str) -> AppResult<QueryResult> {
    let result = database.execute_with_options(
        "MATCH (p:Package {package_id: $id}) RETURN p.package_id, p.name, p.version, p.reviewed",
        &parameters([("id", json!(id))]),
        &options(),
    )?;
    if result.rows.len() != 1 {
        return Err(format!("package not found or not unique: {id}").into());
    }
    Ok(result)
}

pub fn neighbourhood(database: &mut Database, id: &str, reverse: bool) -> AppResult<Value> {
    lookup(database, id)?;
    let result = database.execute_with_options(
        if reverse { DEPENDENTS } else { DEPENDENCIES },
        &parameters([("id", json!(id))]),
        &options(),
    )?;
    let packages = result
        .rows
        .iter()
        .map(|row| {
            Ok(json!({
                "id": text(&row[0])?, "name": text(&row[1])?, "version": text(&row[2])?,
            }))
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(json!(packages))
}

pub fn shortest_path(database: &mut Database, source: &str, target: &str) -> AppResult<Value> {
    lookup(database, source)?;
    lookup(database, target)?;
    let result = database.execute_with_options(
        "MATCH p = shortestPath((:Package {package_id: $source})-[:DEPENDS_ON*0..]->(:Package {package_id: $target}))
         RETURN length(p), nodes(p)",
        &parameters([("source", json!(source)), ("target", json!(target))]), &options(),
    )?;
    let Some(row) = result.rows.first() else {
        return Ok(Value::Null);
    };
    let packages = row[1]
        .as_list()
        .ok_or("path nodes were not a list")?
        .iter()
        .map(|value| {
            let node = value.as_node().ok_or("path contained a non-node")?;
            text(
                node.properties
                    .get("package_id")
                    .ok_or("package ID missing from path")?,
            )
            .map(str::to_owned)
        })
        .collect::<AppResult<Vec<_>>>()?;
    Ok(
        json!({"hops": row[0].as_i64().ok_or("path length was not an integer")?, "packages": packages}),
    )
}

pub fn review(database: &mut Database, id: &str) -> AppResult<()> {
    let mut transaction = database.transaction()?;
    let result = transaction.execute_with_options(
        "MATCH (p:Package {package_id: $id}) SET p.reviewed = true RETURN p.package_id",
        &parameters([("id", json!(id))]),
        &options(),
    )?;
    if result.rows.len() != 1 {
        return Err(format!("package not found or not unique: {id}").into());
    }
    transaction.commit()?;
    Ok(())
}

pub fn delete(database: &mut Database, id: &str) -> AppResult<Value> {
    let summary = database
        .execute_with_options(
            "MATCH (p:Package {package_id: $id}) DETACH DELETE p",
            &parameters([("id", json!(id))]),
            &options(),
        )?
        .summary;
    Ok(
        json!({"nodes_deleted": summary.nodes_deleted, "edges_deleted": summary.relationships_deleted}),
    )
}

/// Deliberately exceed a result budget after staging an application update.
pub fn rollback_probe(database: &mut Database, id: &str) -> AppResult<()> {
    let mut transaction = database.transaction()?;
    transaction.execute_with_options(
        "MATCH (p:Package {package_id: $id}) SET p.reviewed = false",
        &parameters([("id", json!(id))]),
        &options(),
    )?;
    let controls = QueryOptions {
        max_result_rows: Some(0),
        ..options()
    };
    match transaction.execute_with_options(
        "CREATE (p:RollbackProbe) RETURN p",
        &Parameters::new(),
        &controls,
    ) {
        Err(Error::ResourceLimit(_)) => {}
        other => {
            return Err(format!("expected a resource failure after a write, got {other:?}").into());
        }
    }
    if !matches!(transaction.commit(), Err(Error::TransactionAborted)) {
        return Err("failed query did not abort the application transaction".into());
    }
    let probe = database.execute_with_options(
        "MATCH (p:RollbackProbe) RETURN count(p)",
        &Parameters::new(),
        &options(),
    )?;
    if probe.rows[0][0].as_i64() != Some(0) {
        return Err("rollback left a temporary node".into());
    }
    Ok(())
}

/// Sorted bags retain duplicates and opaque IDs, so repeat-import validation
/// detects both duplicate relationships and unnecessary identity replacement.
#[derive(Debug, PartialEq)]
pub struct GraphState {
    pub packages: Vec<Vec<CypherValue>>,
    pub dependencies: Vec<Vec<CypherValue>>,
}

pub fn state(database: &mut Database) -> AppResult<GraphState> {
    Ok(GraphState {
        packages: database.execute_with_options(
            "MATCH (p:Package) RETURN p.package_id, p.name, p.version, p.workspace, p.reviewed, elementId(p) ORDER BY p.package_id",
            &Parameters::new(), &options())?.rows,
        dependencies: database.execute_with_options(
            "MATCH (a)-[r:DEPENDS_ON]->(b) RETURN a.package_id, b.package_id, elementId(r) ORDER BY a.package_id, b.package_id, elementId(r)",
            &Parameters::new(), &options())?.rows,
    })
}

impl GraphState {
    pub fn check_snapshot(&self, snapshot: &Snapshot) -> AppResult<()> {
        let mut expected: Vec<_> = snapshot
            .packages
            .iter()
            .map(|p| {
                vec![
                    CypherValue::String(p.id.clone()),
                    CypherValue::String(p.name.clone()),
                    CypherValue::String(p.version.clone()),
                    CypherValue::Boolean(p.workspace),
                ]
            })
            .collect();
        expected.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
        let actual: Vec<_> = self.packages.iter().map(|row| row[..4].to_vec()).collect();
        if actual != expected {
            return Err("stored package bag differs from the input snapshot".into());
        }
        let mut expected: Vec<_> = snapshot
            .dependencies
            .iter()
            .map(|edge| (edge.source.as_str(), edge.target.as_str()))
            .collect();
        expected.sort();
        let actual = self
            .dependencies
            .iter()
            .map(|row| Ok((text(&row[0])?, text(&row[1])?)))
            .collect::<AppResult<Vec<_>>>()?;
        if actual != expected {
            return Err("stored dependency bag differs from the input snapshot".into());
        }
        Ok(())
    }
}

fn text(value: &CypherValue) -> AppResult<&str> {
    value
        .as_str()
        .ok_or_else(|| "query returned a non-string package field".into())
}
