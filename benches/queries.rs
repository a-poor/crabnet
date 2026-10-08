//! Criterion benchmarks for planning, reads, traversal, writes, and `MERGE`.
//!
//! Run everything with `cargo bench --bench queries`, or filter by benchmark
//! id, for example `cargo bench --bench queries -- 'read/.*/1000'`. The social
//! graph sizes come from `CRABNET_BENCH_SIZES` (comma-separated node counts,
//! default `1000,10000`).

use std::{cell::RefCell, hint::black_box, time::Duration};

use crabnet::{CypherValue, Database, Parameters, QueryResult};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde_json::json;

/// Nodes created by one multi-pattern `CREATE`; also the community size.
const COMMUNITY: usize = 250;
/// Outgoing `KNOWS` edges per person inside its community.
const DEGREE: usize = 4;
/// Distinct `city` values, so grouping has a modest number of keys.
const CITIES: usize = 50;
/// Length of the linear `NEXT` chain used for the traversal group.
const CHAIN: usize = 1_000;

fn sizes() -> Vec<usize> {
    std::env::var("CRABNET_BENCH_SIZES")
        .ok()
        .map(|text| {
            text.split(',')
                .map(|size| {
                    size.trim()
                        .parse()
                        .expect("CRABNET_BENCH_SIZES: integer list")
                })
                .collect()
        })
        .unwrap_or_else(|| vec![1_000, 10_000])
}

/// A small deterministic xorshift generator, so fixtures need no extra crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn params<const N: usize>(values: [(&str, serde_json::Value); N]) -> Parameters {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

fn run(database: &mut Database, cypher: &str) -> QueryResult {
    database
        .execute(cypher)
        .unwrap_or_else(|error| panic!("{cypher}\n{error}"))
}

fn run_with(database: &mut Database, cypher: &str, parameters: &Parameters) -> QueryResult {
    database
        .execute_with(cypher, parameters)
        .unwrap_or_else(|error| panic!("{cypher}\n{error}"))
}

/// Builds `nodes` people in communities of [`COMMUNITY`], each with [`DEGREE`]
/// intra-community `KNOWS` edges plus two bridge edges per community.
///
/// Communities are created in one statement each because planning a
/// multi-pattern `CREATE` grows quadratically with its pattern count, and
/// bridges use matched writes because they cross statement boundaries.
fn social_graph(nodes: usize) -> Database {
    assert!(
        nodes.is_multiple_of(COMMUNITY),
        "node count must be a multiple of {COMMUNITY}"
    );
    let mut database = Database::in_memory().unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    for community in 0..nodes / COMMUNITY {
        let base = community * COMMUNITY;
        let mut patterns = Vec::with_capacity(COMMUNITY * (DEGREE + 1));
        for local in 0..COMMUNITY {
            let id = base + local;
            let score = (rng.below(10_000) as f64) / 100.0;
            patterns.push(format!(
                "(n{local}:Person {{id: {id}, name: 'person-{id}', city: 'city-{}', score: {score}}})",
                id % CITIES
            ));
        }
        for local in 0..COMMUNITY {
            for weight in 0..DEGREE {
                let mut target = rng.below(COMMUNITY);
                if target == local {
                    target = (target + 1) % COMMUNITY;
                }
                patterns.push(format!(
                    "(n{local})-[:KNOWS {{weight: {weight}}}]->(n{target})"
                ));
            }
        }
        run(&mut database, &format!("CREATE {}", patterns.join(", ")));
    }

    let communities = nodes / COMMUNITY;
    for community in 0..communities {
        let base = community * COMMUNITY;
        let next = ((community + 1) % communities) * COMMUNITY;
        let random = rng.below(nodes);
        for target in [next, random] {
            run_with(
                &mut database,
                "MATCH (a:Person {id: $from}), (b:Person {id: $to}) \
                 CREATE (a)-[:KNOWS {weight: 9}]->(b)",
                &params([("from", json!(base)), ("to", json!(target))]),
            );
        }
    }
    database
}

/// Builds a linear chain `(:Step {i: 0})-[:NEXT]->(:Step {i: 1})-> ...` of
/// [`CHAIN`] nodes. Its single path per depth keeps deep, open-ended, and
/// above-threshold traversals from exploding combinatorially.
fn chain_graph() -> Database {
    let mut database = Database::in_memory().unwrap();
    for start in (0..CHAIN).step_by(COMMUNITY) {
        let end = (start + COMMUNITY).min(CHAIN);
        let mut patterns: Vec<String> = (start..end)
            .map(|i| format!("(s{i}:Step {{i: {i}}})"))
            .collect();
        patterns.extend((start..end - 1).map(|i| format!("(s{i})-[:NEXT]->(s{})", i + 1)));
        run(&mut database, &format!("CREATE {}", patterns.join(", ")));
        if start > 0 {
            run_with(
                &mut database,
                "MATCH (a:Step {i: $from}), (b:Step {i: $to}) CREATE (a)-[:NEXT]->(b)",
                &params([("from", json!(start - 1)), ("to", json!(start))]),
            );
        }
    }
    database
}

/// The value a traversal case must produce on the chain fixture.
enum Expect {
    Integer(i64),
    Rows(usize),
}

fn integer(result: &QueryResult) -> i64 {
    match &result.rows[0][0] {
        CypherValue::Integer(value) => *value,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

fn bench_planning(c: &mut Criterion) {
    let mut group = c.benchmark_group("plan");

    let reads = [
        ("point_lookup", "MATCH (p:Person {id: $id}) RETURN p.name"),
        (
            "three_hop_filtered",
            "MATCH (a:Person {id: $id})-[:KNOWS]->(b)-[:KNOWS]->(c)-[:KNOWS]->(d) \
             WHERE d.score > 50 AND NOT d.city IN ['city-1', 'city-2'] \
             RETURN d.name, count(*) AS n ORDER BY n DESC LIMIT 10",
        ),
        (
            "with_pipeline",
            "MATCH (p:Person) WHERE p.id < 100 \
             WITH p ORDER BY p.score DESC LIMIT 20 \
             MATCH (p)-[:KNOWS]->(q) \
             RETURN p.name, collect(q.name) AS friends",
        ),
        (
            "shortest_path",
            "MATCH p = shortestPath((a:Person {id: $a})-[:KNOWS*]->(b:Person {id: $b})) \
             RETURN length(p)",
        ),
    ];
    for (name, cypher) in reads {
        crabnet::explain_physical(cypher).unwrap();
        group.bench_function(BenchmarkId::new("explain_physical", name), |b| {
            b.iter(|| crabnet::explain_physical(black_box(cypher)).unwrap())
        });
    }

    // Mutations only have a logical explanation.
    let merge = "MATCH (a:Person {id: $from}), (b:Person {id: $to}) \
                 MERGE (a)-[r:KNOWS]->(b) \
                 ON CREATE SET r.weight = 1 ON MATCH SET r.weight = r.weight + 1 \
                 RETURN r.weight";
    crabnet::explain(merge).unwrap();
    group.bench_function(BenchmarkId::new("explain", "merge_relationship"), |b| {
        b.iter(|| crabnet::explain(black_box(merge)).unwrap())
    });

    // Multi-pattern CREATE planning: today this is quadratic in pattern count.
    for patterns in [10usize, 100, 250] {
        let cypher = format!(
            "CREATE {}",
            (0..patterns)
                .map(|i| format!("(n{i}:Person {{id: {i}}})"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        group.throughput(Throughput::Elements(patterns as u64));
        group.bench_with_input(
            BenchmarkId::new("explain_create_patterns", patterns),
            &cypher,
            |b, cypher| b.iter(|| crabnet::explain(black_box(cypher)).unwrap()),
        );
    }

    let mut database = Database::in_memory().unwrap();
    group.throughput(Throughput::Elements(1));
    group.bench_function("execute_return_literal", |b| {
        b.iter(|| run(&mut database, black_box("RETURN 1 AS one")))
    });
    group.bench_function("execute_unwind_range_1000", |b| {
        b.iter(|| {
            run(
                &mut database,
                black_box("UNWIND range(1, 1000) AS x RETURN sum(x)"),
            )
        })
    });
    group.finish();
}

fn bench_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("read");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(6));

    for nodes in sizes() {
        let mut database = social_graph(nodes);
        let last = nodes - 1;
        let point = params([("id", json!(nodes / 2))]);
        let pair = params([("a", json!(0)), ("b", json!(last))]);

        let whole_graph: [(&str, &str, Option<&Parameters>); 8] = [
            ("count_nodes", "MATCH (p:Person) RETURN count(p)", None),
            (
                "scan_properties",
                "MATCH (p:Person) RETURN p.name, p.city",
                None,
            ),
            ("scan_nodes", "MATCH (p:Person) RETURN p", None),
            (
                "count_edges",
                "MATCH (:Person)-[k:KNOWS]->(:Person) RETURN count(k)",
                None,
            ),
            (
                "group_by_city",
                "MATCH (p:Person)-[:KNOWS]->(q) \
                 RETURN p.city AS city, count(q) AS n ORDER BY n DESC, city",
                None,
            ),
            (
                "distinct_city",
                "MATCH (p:Person)-[:KNOWS]->(q) WHERE q.score > 50 RETURN DISTINCT q.city",
                None,
            ),
            (
                "order_limit",
                "MATCH (p:Person) RETURN p.name ORDER BY p.score DESC LIMIT 10",
                None,
            ),
            (
                "with_pipeline",
                "MATCH (p:Person) WHERE p.id < 100 \
                 WITH p ORDER BY p.score DESC LIMIT 20 \
                 MATCH (p)-[:KNOWS]->(q) \
                 RETURN p.name, collect(q.name) AS friends",
                None,
            ),
        ];
        let seeded: [(&str, &str, Option<&Parameters>); 8] = [
            (
                "point_lookup",
                "MATCH (p:Person {id: $id}) RETURN p.name",
                Some(&point),
            ),
            (
                "one_hop",
                "MATCH (p:Person {id: $id})-[:KNOWS]->(q) RETURN q.name",
                Some(&point),
            ),
            (
                "two_hop_count",
                "MATCH (p:Person {id: $id})-[:KNOWS]->()-[:KNOWS]->(q) RETURN count(q)",
                Some(&point),
            ),
            (
                "three_hop_count",
                "MATCH (p:Person {id: $id})-[:KNOWS]->()-[:KNOWS]->()-[:KNOWS]->(q) \
                 RETURN count(q)",
                Some(&point),
            ),
            (
                "var_len_1_3_count",
                "MATCH (p:Person {id: $id})-[:KNOWS*1..3]->(q) RETURN count(q)",
                Some(&point),
            ),
            (
                "var_len_1_2_paths",
                "MATCH path = (p:Person {id: $id})-[:KNOWS*1..2]->(q) RETURN path",
                Some(&point),
            ),
            (
                "shortest_path_length",
                "MATCH p = shortestPath((a:Person {id: $a})-[:KNOWS*]->(b:Person {id: $b})) \
                 RETURN length(p)",
                Some(&pair),
            ),
            (
                "union_neighbours",
                "MATCH (p:Person {id: $a})-[:KNOWS]->(q) RETURN q.name \
                 UNION MATCH (p:Person {id: $b})-[:KNOWS]->(q) RETURN q.name",
                Some(&pair),
            ),
        ];

        for (name, cypher, parameters) in whole_graph.into_iter().chain(seeded) {
            let empty = Parameters::new();
            let parameters = parameters.unwrap_or(&empty);
            // Fail loudly on an unsupported query before timing it.
            let result = run_with(&mut database, cypher, parameters);
            black_box(&result);
            group.throughput(Throughput::Elements(nodes as u64));
            group.bench_with_input(BenchmarkId::new(name, nodes), &nodes, |b, _| {
                b.iter(|| run_with(&mut database, black_box(cypher), parameters))
            });
        }
        let scan = "MATCH (p:Person) RETURN p.name, p.city";
        let union = format!("{scan} UNION ALL {scan}");
        assert_eq!(run(&mut database, &union).rows.len(), nodes * 2);
        group.throughput(Throughput::Elements((nodes * 2) as u64));
        group.bench_function(BenchmarkId::new("union_all_scan_properties", nodes), |b| {
            b.iter(|| run(&mut database, black_box(&union)));
        });
        let distinct = "MATCH (p:Person) RETURN DISTINCT p.city AS city";
        let union_distinct =
            "MATCH (p:Person) RETURN p.city AS city UNION MATCH (p:Person) RETURN p.city AS city";
        let cities = run(&mut database, distinct).rows.len();
        let paths = "MATCH p=(:Person)-[:KNOWS]->() RETURN p";
        let path_nodes = "MATCH p=(:Person)-[:KNOWS]->() RETURN nodes(p)";
        let edges = nodes * DEGREE + nodes / COMMUNITY * 2;
        for (name, query) in [("hydrate_paths", paths), ("hydrate_nodes", path_nodes)] {
            assert_eq!(run(&mut database, query).rows.len(), edges);
            group.throughput(Throughput::Elements(edges as u64));
            group.bench_function(BenchmarkId::new(name, nodes), |b| {
                b.iter(|| run(&mut database, black_box(query)));
            });
        }
        for (scan, row_count, scan_name, first_name) in [
            (scan, nodes, "cursor_scan_properties", "cursor_first_row"),
            (
                union.as_str(),
                nodes * 2,
                "cursor_union_all_scan_properties",
                "cursor_union_all_first_row",
            ),
            (
                distinct,
                cities,
                "cursor_distinct_city",
                "cursor_distinct_first_row",
            ),
            (
                union_distinct,
                cities,
                "cursor_union_distinct_city",
                "cursor_union_distinct_first_row",
            ),
            (
                paths,
                edges,
                "cursor_hydrate_paths",
                "cursor_hydrate_paths_first_row",
            ),
            (
                path_nodes,
                edges,
                "cursor_hydrate_nodes",
                "cursor_hydrate_nodes_first_row",
            ),
        ] {
            {
                let mut read = database.prepare_read(scan).unwrap();
                assert_eq!(read.mode(), crabnet::ReadMode::Incremental);
                let count = read
                    .cursor()
                    .unwrap()
                    .try_fold(0, |count, row| row.map(|_| count + 1))
                    .unwrap();
                assert_eq!(count, row_count);
            }
            for first_only in [false, true] {
                let name = if first_only { first_name } else { scan_name };
                group.throughput(Throughput::Elements(if first_only {
                    1
                } else {
                    row_count as u64
                }));
                group.bench_function(BenchmarkId::new(name, nodes), |b| {
                    b.iter(|| {
                        let mut read = database.prepare_read(black_box(scan)).unwrap();
                        let mut cursor = read.cursor().unwrap();
                        if first_only {
                            black_box(cursor.next().unwrap().unwrap());
                        } else {
                            for row in cursor {
                                black_box(row.unwrap());
                            }
                        }
                    });
                });
            }
        }
    }
    group.finish();
}

fn bench_traversal(c: &mut Criterion) {
    let mut group = c.benchmark_group("traversal");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(6));
    group.throughput(Throughput::Elements(CHAIN as u64));

    let mut database = chain_graph();
    let start = params([("i", json!(0))]);
    let ends = params([("from", json!(0)), ("to", json!(CHAIN - 1))]);

    let cases: [(&str, &str, &Parameters, Expect); 7] = [
        // Bounds at or below 32 hops compile to a SQLite recursive CTE.
        (
            "sql_bounded_1_30",
            "MATCH (s:Step {i: $i})-[:NEXT*1..30]->(e) RETURN count(e)",
            &start,
            Expect::Integer(30),
        ),
        // Bounds above 32 hops run the Rust trail enumerator.
        (
            "rust_bounded_1_40",
            "MATCH (s:Step {i: $i})-[:NEXT*1..40]->(e) RETURN count(e)",
            &start,
            Expect::Integer(40),
        ),
        (
            "rust_open_ended",
            "MATCH (s:Step {i: $i})-[:NEXT*]->(e) RETURN count(e)",
            &start,
            Expect::Integer((CHAIN - 1) as i64),
        ),
        (
            "rust_open_ended_from_200",
            "MATCH (s:Step {i: $i})-[:NEXT*200..]->(e) RETURN count(e)",
            &start,
            Expect::Integer((CHAIN - 200) as i64),
        ),
        (
            "shortest_path_whole_chain",
            "MATCH p = shortestPath((a:Step {i: $from})-[:NEXT*]->(b:Step {i: $to})) \
             RETURN length(p)",
            &ends,
            Expect::Integer((CHAIN - 1) as i64),
        ),
        (
            "hydrate_paths_to_10",
            "MATCH p = (s:Step {i: $i})-[:NEXT*..10]->(e) RETURN p",
            &start,
            Expect::Rows(10),
        ),
        (
            "relationship_lists_to_10",
            "MATCH (s:Step {i: $i})-[rels:NEXT*..10]->(e) RETURN rels",
            &start,
            Expect::Rows(10),
        ),
    ];

    for (name, cypher, parameters, expected) in cases {
        // Each count and row total is fixed by the chain shape, so a wrong
        // value means the benchmark no longer measures what its name says.
        let result = run_with(&mut database, cypher, parameters);
        match expected {
            Expect::Integer(value) => assert_eq!(integer(&result), value, "{name}: {result:?}"),
            Expect::Rows(rows) => assert_eq!(result.rows.len(), rows, "{name}: {result:?}"),
        }
        group.bench_function(name, |b| {
            b.iter(|| run_with(&mut database, black_box(cypher), parameters))
        });
    }
    group.finish();
}

fn bench_writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("write");
    group.throughput(Throughput::Elements(1));

    group.bench_function("create_node_literal", |b| {
        b.iter_batched(
            || Database::in_memory().unwrap(),
            |mut database| {
                run(
                    &mut database,
                    "CREATE (:Person {id: 1, name: 'Ada', tags: ['math', 'poetry']})",
                )
            },
            BatchSize::SmallInput,
        )
    });

    let node_params = params([
        ("id", json!(1)),
        ("name", json!("Ada")),
        ("tags", json!(["math", "poetry"])),
    ]);
    group.bench_function("create_node_parameters", |b| {
        b.iter_batched(
            || Database::in_memory().unwrap(),
            |mut database| {
                run_with(
                    &mut database,
                    "CREATE (:Person {id: $id, name: $name, tags: $tags})",
                    &node_params,
                )
            },
            BatchSize::SmallInput,
        )
    });

    let batch = format!(
        "CREATE {}",
        (0..100)
            .map(|i| format!("(n{i}:Person {{id: {i}, name: 'person-{i}'}})"))
            .chain((0..99).map(|i| format!("(n{i})-[:KNOWS]->(n{})", i + 1)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    group.throughput(Throughput::Elements(199));
    group.bench_function("create_batch_100_nodes_99_edges", |b| {
        b.iter_batched(
            || Database::in_memory().unwrap(),
            |mut database| run(&mut database, &batch),
            BatchSize::SmallInput,
        )
    });

    // Matched writes run against a populated graph. Each iteration adds one
    // edge or rewrites one property, which is negligible next to the fixture.
    let nodes = sizes().into_iter().min().unwrap_or(1_000);
    let database = RefCell::new(social_graph(nodes));
    group.throughput(Throughput::Elements(1));
    let mut counter = 0usize;
    group.bench_function(
        BenchmarkId::new("create_relationship_matched", nodes),
        |b| {
            b.iter(|| {
                counter += 1;
                let from = counter % nodes;
                let to = (counter * 7 + 1) % nodes;
                run_with(
                    &mut database.borrow_mut(),
                    "MATCH (a:Person {id: $from}), (b:Person {id: $to}) \
                 CREATE (a)-[:KNOWS {weight: 1}]->(b)",
                    &params([("from", json!(from)), ("to", json!(to))]),
                )
            })
        },
    );
    group.bench_function(BenchmarkId::new("set_property_matched", nodes), |b| {
        b.iter(|| {
            counter += 1;
            run_with(
                &mut database.borrow_mut(),
                "MATCH (p:Person {id: $id}) SET p.score = $score",
                &params([("id", json!(counter % nodes)), ("score", json!(counter))]),
            )
        })
    });
    group.bench_function(BenchmarkId::new("set_property_map_merge", nodes), |b| {
        b.iter(|| {
            counter += 1;
            run_with(
                &mut database.borrow_mut(),
                "MATCH (p:Person {id: $id}) SET p += $patch",
                &params([
                    ("id", json!(counter % nodes)),
                    ("patch", json!({"visits": counter, "flag": true})),
                ]),
            )
        })
    });
    group.bench_function(BenchmarkId::new("detach_delete_matched", nodes), |b| {
        b.iter_batched(
            || {
                counter += 1;
                let id = 1_000_000 + counter;
                run_with(
                    &mut database.borrow_mut(),
                    "MATCH (anchor:Person {id: $anchor}) \
                     CREATE (anchor)-[:KNOWS]->(:Person {id: $id, name: 'temp'})",
                    &params([("anchor", json!(counter % nodes)), ("id", json!(id))]),
                );
                id
            },
            |id| {
                run_with(
                    &mut database.borrow_mut(),
                    "MATCH (p:Person {id: $id}) DETACH DELETE p",
                    &params([("id", json!(id))]),
                )
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

/// Builds `nodes` people with no relationships, in chunks of [`COMMUNITY`].
/// Cheap enough to rebuild per iteration for merges that grow the graph.
fn people_only(nodes: usize) -> Database {
    let mut database = Database::in_memory().unwrap();
    for start in (0..nodes).step_by(COMMUNITY) {
        let patterns: Vec<String> = (start..(start + COMMUNITY).min(nodes))
            .map(|id| {
                format!(
                    "(n{id}:Person {{id: {id}, name: 'person-{id}', city: 'city-{}'}})",
                    id % CITIES
                )
            })
            .collect();
        run(&mut database, &format!("CREATE {}", patterns.join(", ")));
    }
    database
}

fn bench_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge");
    group.throughput(Throughput::Elements(1));

    // Single-row merges against a populated graph: one that always matches,
    // one that always creates, and a relationship merge between matched nodes.
    let nodes = sizes().into_iter().min().unwrap_or(1_000);
    let database = RefCell::new(social_graph(nodes));
    let mut counter = 0usize;
    group.bench_function(BenchmarkId::new("node_matched", nodes), |b| {
        b.iter(|| {
            counter += 1;
            run_with(
                &mut database.borrow_mut(),
                "MERGE (p:Person {id: $id}) ON MATCH SET p.seen = $seen RETURN p.name",
                &params([("id", json!(counter % nodes)), ("seen", json!(counter))]),
            )
        })
    });
    // The setup removes the previous iteration's node so the label scan the
    // merge matcher runs does not grow across the measurement. Setups and
    // routines must alternate, so this cannot use a batched size.
    group.bench_function(BenchmarkId::new("node_created", nodes), |b| {
        b.iter_batched(
            || {
                run_with(
                    &mut database.borrow_mut(),
                    "MATCH (p:Person {id: $id}) DETACH DELETE p",
                    &params([("id", json!(1_000_000 + counter))]),
                );
                counter += 1;
                params([
                    ("id", json!(1_000_000 + counter)),
                    ("name", json!(format!("merged-{counter}"))),
                ])
            },
            |parameters| {
                run_with(
                    &mut database.borrow_mut(),
                    "MERGE (p:Person {id: $id}) ON CREATE SET p.name = $name RETURN p.name",
                    &parameters,
                )
            },
            BatchSize::PerIteration,
        )
    });
    group.bench_function(BenchmarkId::new("relationship_matched", nodes), |b| {
        b.iter(|| {
            counter += 1;
            let from = counter % nodes;
            let to = (counter * 7 + 1) % nodes;
            run_with(
                &mut database.borrow_mut(),
                "MATCH (a:Person {id: $from}), (b:Person {id: $to}) \
                 MERGE (a)-[r:KNOWS]->(b) \
                 ON CREATE SET r.weight = 1 ON MATCH SET r.weight = r.weight + 1",
                &params([("from", json!(from)), ("to", json!(to))]),
            )
        })
    });

    // Fan-out merges: one MERGE fed by every person. `shared` collapses the
    // rows onto CITIES keys, so nearly every row matches something an
    // earlier row created; `distinct` creates once per row, which is the
    // worst case for the row-at-a-time create loop. Both rebuild the fixture
    // per iteration because the merge changes the graph.
    group.sample_size(10);
    let fanouts = [
        (
            "fanout_shared",
            "MATCH (p:Person) MERGE (c:City {name: p.city}) \
             MERGE (p)-[:LIVES_IN]->(c)",
            [250usize, 1_000],
        ),
        (
            "fanout_distinct",
            "MATCH (p:Person) MERGE (s:Shadow {id: p.id}) \
             ON CREATE SET s.name = p.name",
            [250, 1_000],
        ),
    ];
    for (name, cypher, sizes) in fanouts {
        for rows in sizes {
            let mut warm = people_only(rows);
            let result = run(&mut warm, cypher);
            assert_eq!(result.rows.len(), 0, "{name} should not return rows");
            group.throughput(Throughput::Elements(rows as u64));
            group.bench_with_input(BenchmarkId::new(name, rows), &rows, |b, &rows| {
                b.iter_batched(
                    || people_only(rows),
                    |mut database| run(&mut database, cypher),
                    BatchSize::SmallInput,
                )
            });
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_planning,
    bench_reads,
    bench_traversal,
    bench_writes,
    bench_merge
);
criterion_main!(benches);
