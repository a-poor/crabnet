//! A disk-backed Cargo dependency explorer and reproducible lifecycle workload.
#[path = "dependency_graph/model.rs"]
mod model;
#[path = "dependency_graph/store.rs"]
mod store;

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

use std::{fs, path::Path, time::Instant};

use crabnet::{Database, OpenMode, OpenOptions, Parameters};
use serde_json::{Value, json};

const USAGE: &str = "Dependency graph example (JSON output)
  dependency_graph import DATABASE CARGO_METADATA_JSON [BATCH_SIZE=128]
  dependency_graph dependencies DATABASE PACKAGE_ID    (up to four hops)
  dependency_graph dependents DATABASE PACKAGE_ID      (up to four hops)
  dependency_graph path DATABASE SOURCE_ID TARGET_ID   (shortest directed path)
  dependency_graph review DATABASE PACKAGE_ID
  dependency_graph remove DATABASE PACKAGE_ID
  dependency_graph demo NEW_DATABASE [PACKAGES=1000] [SAMPLES=20] [BATCH_SIZE=128]

Create metadata with: cargo metadata --format-version 1 > metadata.json
Import synchronizes a complete snapshot, including deleting stale packages and edges.
Use a dedicated database for this example. Demo refuses to overwrite an existing file.";

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!("{USAGE}");
        return;
    }
    match command(&args) {
        Ok(report) => println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("JSON report")
        ),
        Err(error) => {
            eprintln!("dependency_graph: {error}");
            std::process::exit(1);
        }
    }
}

fn command(args: &[String]) -> AppResult<Value> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(USAGE.into());
    };
    match command {
        "demo" if (2..=5).contains(&args.len()) => demo(
            Path::new(&args[1]),
            number(args.get(2), 1000)?,
            number(args.get(3), 20)?,
            number(args.get(4), 128)?,
        ),
        "import" if (3..=4).contains(&args.len()) => {
            let snapshot =
                model::Snapshot::from_metadata(&serde_json::from_slice(&fs::read(&args[2])?)?)?;
            let batch_size = number(args.get(3), 128)?;
            validate_batch_size(batch_size)?;
            let mut database = Database::open(&args[1])?;
            check_owner(&mut database, true)?;
            let import = store::import(&mut database, &snapshot, batch_size, &store::options())?;
            store::state(&mut database)?.check_snapshot(&snapshot)?;
            Ok(
                json!({"packages": snapshot.packages.len(), "dependencies": snapshot.dependencies.len(), "import": import}),
            )
        }
        "dependencies" | "dependents" | "review" | "remove" if args.len() == 3 => {
            let mode = if matches!(command, "dependencies" | "dependents") {
                OpenMode::ReadOnly
            } else {
                OpenMode::ReadWrite
            };
            let mut database = open_existing(Path::new(&args[1]), mode)?;
            match command {
                "dependencies" => store::neighbourhood(&mut database, &args[2], false),
                "dependents" => store::neighbourhood(&mut database, &args[2], true),
                "review" => {
                    store::review(&mut database, &args[2])?;
                    Ok(json!({"reviewed": args[2]}))
                }
                _ => store::delete(&mut database, &args[2]),
            }
        }
        "path" if args.len() == 4 => store::shortest_path(
            &mut open_existing(Path::new(&args[1]), OpenMode::ReadOnly)?,
            &args[2],
            &args[3],
        ),
        _ => Err(USAGE.into()),
    }
}

fn number(value: Option<&String>, default: usize) -> AppResult<usize> {
    value.map_or(Ok(default), |value| Ok(value.parse()?))
}

fn validate_batch_size(batch_size: usize) -> AppResult<()> {
    if !(1..=1024).contains(&batch_size) {
        return Err("batch size must be between 1 and 1024".into());
    }
    Ok(())
}

fn check_owner(database: &mut Database, allow_empty: bool) -> AppResult<()> {
    let marker = database.execute_with_options(
        "MATCH (s:DependencyImport {key: 'cargo'}) RETURN s.generation",
        &Parameters::new(),
        &store::options(),
    )?;
    if marker.rows.len() == 1 && marker.rows[0][0].as_i64().is_some_and(|n| n > 0) {
        return Ok(());
    }
    if allow_empty
        && database
            .execute_with_options(
                "MATCH (n) RETURN count(n)",
                &Parameters::new(),
                &store::options(),
            )?
            .rows[0][0]
            .as_i64()
            == Some(0)
    {
        return Ok(());
    }
    Err("database is not an initialized dependency-graph example; use a dedicated database".into())
}

fn open_existing(path: &Path, mode: OpenMode) -> AppResult<Database> {
    let mut database = Database::open_with_options(
        path,
        &OpenOptions {
            mode,
            ..OpenOptions::default()
        },
    )?;
    check_owner(&mut database, false)?;
    Ok(database)
}

fn demo(path: &Path, package_count: usize, samples: usize, batch_size: usize) -> AppResult<Value> {
    validate_batch_size(batch_size)?;
    if !(1..=10_000).contains(&samples) {
        return Err("samples must be between 1 and 10000".into());
    }
    let mut snapshot = model::Snapshot::generated(package_count)?;
    // Reserve a fresh path without truncating an existing file.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let open_started = Instant::now();
    let mut database = Database::open(path)?;
    let open_ms = milliseconds(open_started);
    let first_import = store::import(&mut database, &snapshot, batch_size, &store::options())?;
    let original = store::state(&mut database)?;
    original.check_snapshot(&snapshot)?;
    let repeat_import = store::import(&mut database, &snapshot, batch_size, &store::options())?;
    if original != store::state(&mut database)? {
        return Err("repeat import changed graph values or identities".into());
    }
    drop(original);

    let source = snapshot.packages[0].id.clone();
    let target = snapshot.packages[63.min(package_count - 1)].id.clone();
    let lookup = measure(samples, || {
        Ok(json!(store::lookup(&mut database, &source)?.rows.len()))
    })?;
    let dependencies = measure(samples, || {
        store::neighbourhood(&mut database, &source, false)
    })?;
    let dependents = measure(samples, || {
        store::neighbourhood(&mut database, &target, true)
    })?;
    let shortest = measure(samples, || {
        store::shortest_path(&mut database, &source, &target)
    })?;
    let target_index = 63.min(package_count - 1);
    let expected_packages = |indices: std::ops::Range<usize>| {
        json!(
            indices
                .map(|index| {
                    let package = &snapshot.packages[index];
                    json!({"id": package.id, "name": package.name, "version": package.version})
                })
                .collect::<Vec<_>>()
        )
    };
    if dependencies["result"] != expected_packages(1..9.min(package_count))
        || dependents["result"] != expected_packages(target_index.saturating_sub(8)..target_index)
        || shortest["result"]["hops"] != target_index.div_ceil(2)
    {
        return Err("query results did not match the generated fixture".into());
    }
    let path_packages = shortest["result"]["packages"]
        .as_array()
        .ok_or("shortest path omitted its packages")?;
    let edges: std::collections::BTreeSet<_> = snapshot
        .dependencies
        .iter()
        .map(|edge| (edge.source.as_str(), edge.target.as_str()))
        .collect();
    if path_packages.len() != target_index.div_ceil(2) + 1
        || path_packages.first() != Some(&json!(source))
        || path_packages.last() != Some(&json!(target))
        || path_packages.windows(2).any(|pair| {
            !edges.contains(&(
                pair[0].as_str().unwrap_or(""),
                pair[1].as_str().unwrap_or(""),
            ))
        })
    {
        return Err("shortest path did not follow the generated dependencies".into());
    }
    drop(edges);

    let update_started = Instant::now();
    store::review(&mut database, &source)?;
    let update_ms = milliseconds(update_started);
    let reviewed = store::state(&mut database)?;
    let rollback_started = Instant::now();
    store::rollback_probe(&mut database, &source)?;
    let rollback_ms = milliseconds(rollback_started);
    if reviewed != store::state(&mut database)? {
        return Err("failed operation changed graph state".into());
    }
    drop(reviewed);

    let removed = snapshot
        .packages
        .pop()
        .expect("generated at least eight packages");
    let expected_deleted_edges = snapshot
        .dependencies
        .iter()
        .filter(|edge| edge.source == removed.id || edge.target == removed.id)
        .count();
    let delete_started = Instant::now();
    let deletion = store::delete(&mut database, &removed.id)?;
    let delete_ms = milliseconds(delete_started);
    if deletion["nodes_deleted"] != 1 || deletion["edges_deleted"] != expected_deleted_edges {
        return Err("delete counters did not match the fixture".into());
    }
    snapshot
        .dependencies
        .retain(|edge| edge.source != removed.id && edge.target != removed.id);
    let final_state = store::state(&mut database)?;
    final_state.check_snapshot(&snapshot)?;
    drop(database);
    let reopen_started = Instant::now();
    let mut database = open_existing(path, OpenMode::ReadOnly)?;
    let reopen_ms = milliseconds(reopen_started);
    if final_state != store::state(&mut database)? {
        return Err("reopened database changed graph values or identities".into());
    }
    if store::lookup(&mut database, &source)?.rows[0][3].as_bool() != Some(true) {
        return Err("review annotation did not survive rollback and reopen".into());
    }
    drop(database);
    Ok(json!({
        "workload_version": 1, "crabnet_version": env!("CARGO_PKG_VERSION"),
        "debug_assertions": cfg!(debug_assertions), "database": path,
        "fixture": {"packages": package_count, "dependencies": snapshot.dependencies.len() + expected_deleted_edges,
            "component_size": 64, "forward_offsets": [1, 2], "batch_size": batch_size},
        "query_ids": {"source": source, "target": target},
        "open_ms": open_ms, "first_import": first_import, "repeat_import": repeat_import,
        "queries": {"lookup": lookup, "dependencies_4_hops": dependencies, "dependents_4_hops": dependents, "shortest_path": shortest},
        "update_ms": update_ms, "rollback_ms": rollback_ms, "delete_ms": delete_ms, "deletion": deletion,
        "reopen_ms": reopen_ms, "database_bytes": fs::metadata(path)?.len(),
        "final_packages": final_state.packages.len(), "final_dependencies": final_state.dependencies.len(),
        "verified": {"repeat_identity": true, "rollback": true, "reopen_identity": true}
    }))
}

fn milliseconds(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn measure(samples: usize, mut operation: impl FnMut() -> AppResult<Value>) -> AppResult<Value> {
    let started = Instant::now();
    let expected = operation()?;
    let first_ms = milliseconds(started);
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let actual = operation()?;
        timings.push(milliseconds(started));
        if actual != expected {
            return Err("repeated query changed its result".into());
        }
    }
    timings.sort_by(f64::total_cmp);
    Ok(
        json!({"samples": samples, "first_ms": first_ms, "p50_ms": timings[samples.div_ceil(2) - 1],
        "p95_ms": timings[(samples * 95).div_ceil(100) - 1], "max_ms": timings[samples - 1], "result": expected}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crabnet::Database;

    #[test]
    fn imports_packages_and_dependency_edges() {
        let mut database = Database::in_memory().unwrap();
        let snapshot = model::Snapshot::generated(8).unwrap();
        store::import(&mut database, &snapshot, 3, &store::options()).unwrap();
        let packages = database
            .execute("MATCH (p:Package) RETURN count(p)")
            .unwrap();
        assert_eq!(packages.rows[0][0].as_i64(), Some(8));
        let edges = database
            .execute("MATCH ()-[r:DEPENDS_ON]->() RETURN count(r)")
            .unwrap();
        assert_eq!(edges.rows[0][0].as_i64(), Some(13));
    }

    #[test]
    fn repeat_import_keeps_ids_and_annotations_and_reconciles_changed_snapshots() -> AppResult<()> {
        let mut database = Database::in_memory()?;
        let mut snapshot = model::Snapshot::generated(8)?;
        store::import(&mut database, &snapshot, 3, &store::options())?;
        store::review(&mut database, &snapshot.packages[0].id)?;
        let before = store::state(&mut database)?;
        let repeat = store::import(&mut database, &snapshot, 3, &store::options())?;
        assert_eq!(repeat["nodes_created"], 0);
        assert_eq!(repeat["edges_created"], 0);
        assert_eq!(before, store::state(&mut database)?);

        let removed = snapshot.packages.pop().unwrap().id;
        snapshot
            .dependencies
            .retain(|edge| edge.source != removed && edge.target != removed);
        snapshot.dependencies.remove(0);
        snapshot.packages[1].name = "renamed".into();
        snapshot.packages[1].version = "2.0.0".into();
        snapshot.packages[1].workspace = true;
        snapshot.packages.push(model::Package {
            id: "new-package".into(),
            name: "new".into(),
            version: "1.0.0".into(),
            workspace: false,
        });
        snapshot.dependencies.push(model::Dependency {
            source: snapshot.packages[0].id.clone(),
            target: "new-package".into(),
        });
        let changed = store::import(&mut database, &snapshot, 2, &store::options())?;
        assert_eq!(changed["nodes_created"], 1);
        assert_eq!(changed["nodes_deleted"], 1);
        assert_eq!(changed["edges_created"], 1);
        assert_eq!(changed["edges_deleted"], 3);
        let after = store::state(&mut database)?;
        after.check_snapshot(&snapshot)?;
        assert_eq!(
            store::lookup(&mut database, &snapshot.packages[0].id)?.rows[0][3].as_bool(),
            Some(true)
        );
        assert_eq!(before.packages[0][5], after.packages[0][5]);
        store::import(
            &mut database,
            &model::Snapshot {
                packages: vec![],
                dependencies: vec![],
            },
            1,
            &store::options(),
        )?;
        let empty = store::state(&mut database)?;
        assert!(empty.packages.is_empty() && empty.dependencies.is_empty());
        Ok(())
    }

    fn metadata() -> Value {
        json!({"version": 1, "workspace_members": ["workspace:a'\n"],
        "packages": [
            {"id": "workspace:a'\n", "name": "app", "version": "1.0.0"},
            {"id": "registry:b@2.0.0", "name": "dep", "version": "2.0.0"}
        ],
        "resolve": {"nodes": [
            {"id": "workspace:a'\n", "dependencies": ["registry:b@2.0.0", "registry:b@2.0.0"]},
            {"id": "registry:b@2.0.0", "dependencies": []}
        ]}})
    }

    #[test]
    fn metadata_ids_are_bound_and_traversal_handles_identity_and_unreachable_pairs() -> AppResult<()>
    {
        let snapshot = model::Snapshot::from_metadata(&metadata())?;
        assert_eq!(snapshot.dependencies.len(), 1);
        let mut database = Database::in_memory()?;
        store::import(&mut database, &snapshot, 128, &store::options())?;
        let source = &snapshot.packages[0].id;
        let target = &snapshot.packages[1].id;
        let dependencies = store::neighbourhood(&mut database, source, false)?;
        assert_eq!(dependencies[0]["id"], *target);
        assert_eq!(
            store::neighbourhood(&mut database, target, true)?[0]["id"],
            *source
        );
        assert_eq!(
            store::shortest_path(&mut database, source, target)?,
            json!({"hops": 1, "packages": [source, target]})
        );
        assert_eq!(
            store::shortest_path(&mut database, source, source)?,
            json!({"hops": 0, "packages": [source]})
        );
        assert!(store::shortest_path(&mut database, target, source)?.is_null());
        assert!(store::neighbourhood(&mut database, "missing", false).is_err());
        assert!(store::review(&mut database, "missing").is_err());
        store::state(&mut database)?.check_snapshot(&snapshot)?;
        Ok(())
    }

    #[test]
    fn invalid_metadata_and_resource_failures_leave_the_previous_import_intact() -> AppResult<()> {
        let mut invalid = metadata();
        invalid["resolve"] = Value::Null;
        assert!(model::Snapshot::from_metadata(&invalid).is_err());
        invalid = metadata();
        invalid["resolve"]["nodes"][0]["dependencies"] = json!(["missing"]);
        assert!(model::Snapshot::from_metadata(&invalid).is_err());
        invalid = metadata();
        invalid["packages"][1]["id"] = invalid["packages"][0]["id"].clone();
        assert!(model::Snapshot::from_metadata(&invalid).is_err());

        let mut database = Database::in_memory()?;
        let snapshot = model::Snapshot::generated(8)?;
        store::import(&mut database, &snapshot, 3, &store::options())?;
        let before = store::state(&mut database)?;
        let mut invalid = snapshot.clone();
        invalid.dependencies[0].target = "missing".into();
        assert!(store::import(&mut database, &invalid, 3, &store::options()).is_err());
        let limited = crabnet::QueryOptions {
            max_materialized_rows: Some(4),
            ..store::options()
        };
        let error = store::import(&mut database, &snapshot, 3, &limited).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<crabnet::Error>(),
                Some(crabnet::Error::ResourceLimit(_))
            ),
            "{error}"
        );
        assert_eq!(before, store::state(&mut database)?);
        assert_eq!(
            store::import(&mut database, &snapshot, 3, &store::options())?["generation"],
            2
        );
        Ok(())
    }

    struct Directory(std::path::PathBuf);

    impl Directory {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "crabnet-dependency-{}-{nanos}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn disk_workload_verifies_rollback_reopen_and_refuses_existing_files() -> AppResult<()> {
        let directory = Directory::new();
        let path = directory.0.join("graph.sqlite");
        let report = demo(&path, 16, 2, 4)?;
        assert_eq!(report["final_packages"], 15);
        assert_eq!(report["final_dependencies"], 27);
        assert_eq!(report["queries"]["shortest_path"]["result"]["hops"], 8);
        assert_eq!(
            report["queries"]["dependencies_4_hops"]["result"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        assert_eq!(
            report["verified"],
            json!({"repeat_identity": true, "rollback": true, "reopen_identity": true})
        );
        let bytes = fs::read(&path)?;
        assert!(demo(&path, 16, 1, 4).is_err());
        assert_eq!(bytes, fs::read(&path)?);
        let absent = directory.0.join("absent.sqlite");
        assert!(open_existing(&absent, OpenMode::ReadOnly).is_err());
        assert!(!absent.exists());
        Ok(())
    }

    #[test]
    fn cli_import_owns_only_a_dedicated_graph_and_preserves_annotations() -> AppResult<()> {
        let directory = Directory::new();
        let path = directory.0.join("graph.sqlite");
        let metadata_path = directory.0.join("metadata.json");
        fs::write(&metadata_path, serde_json::to_vec(&metadata())?)?;
        let import = vec![
            "import".into(),
            path.to_str().unwrap().into(),
            metadata_path.to_str().unwrap().into(),
        ];
        assert_eq!(command(&import)?["dependencies"], 1);
        command(&[
            "review".into(),
            path.to_str().unwrap().into(),
            "workspace:a'\n".into(),
        ])?;
        assert_eq!(command(&import)?["import"]["edges_created"], 0);
        assert_eq!(
            command(&[
                "dependencies".into(),
                path.to_str().unwrap().into(),
                "workspace:a'\n".into(),
            ])?,
            json!([{"id": "registry:b@2.0.0", "name": "dep", "version": "2.0.0"}])
        );
        assert_eq!(
            command(&[
                "path".into(),
                path.to_str().unwrap().into(),
                "workspace:a'\n".into(),
                "registry:b@2.0.0".into(),
            ])?["hops"],
            1
        );
        let mut reopened = open_existing(&path, OpenMode::ReadOnly)?;
        assert_eq!(
            store::lookup(&mut reopened, "workspace:a'\n")?.rows[0][3].as_bool(),
            Some(true)
        );
        let mut foreign = Database::in_memory()?;
        foreign.execute("CREATE (:Unrelated)")?;
        assert!(check_owner(&mut foreign, true).is_err());
        assert_eq!(
            foreign.execute("MATCH (n) RETURN count(n)")?.rows[0][0].as_i64(),
            Some(1)
        );
        Ok(())
    }
}
