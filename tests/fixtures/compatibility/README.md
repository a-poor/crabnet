# Retained storage fixtures

These small SQLite files were written by pinned historical Crabnet commits,
not reconstructed with the current schema constants. All writers declare
package version `0.1.0`; these are main-commit baselines, not published release
fixtures. See the [compatibility policy](../../../docs/compatibility.md).

| Schema | Writer commit | Snapshot method |
| --- | --- | --- |
| 1 | `f2a5b662a46d1d13af724c168bbefe8debfb06f3` | Closed DELETE-journal file copy |
| 2 | `b3c71e825b0bf09a6976b1f702e82091de7628c5` | Closed DELETE-journal file copy |
| 3 | `b409846d3380088998b908d78cb17afb9e89d563` | Closed DELETE-journal file copy |
| 4 | `9453f5b1e7b102f0e53fe6da1b6de2b0bd7b9eed` | Public `backup_to` |
| 5 | `683b8e9022684e64c2b12acf1cb90432c7ecf04d` | Public `backup_to` |

Schemas 1–4 use the last main commit before the next format transition; schema
5 uses the first hosted-CI-qualified commit. `manifest.json` records compiler,
writer/lockfile/producer identities, snapshot method, and fixture/oracle hashes.
The writer reopens its own snapshot before recording it. Historical checkouts
have separate Cargo target directories so same-version crate artifacts cannot
be reused across writers.

The five-node/four-relationship graph includes an unlabeled node, multiple
labels where supported, parallel edges, a cycle, a self-loop, null/missing
properties, booleans, an empty list, nested lists/maps, integer versus float
values, a large exact integer, a precise float, and quoted/non-ASCII text.
Schemas 4–5 contain two uniqueness constraints. `vN.json` captures sorted
persisted nodes, relationships, and declarations with original IDs, using the
historical dependency's SQLite reader. It is independent of the current reader's
hydration or migrations. Sorting preserves multiplicity.

The schema-1/2 writers retain explicit top-level null entries in stored maps;
the later writers omit them. Keep those historical differences in the oracles.
Migration preserves stored entries, while property functions treat top-level
nulls as absent. Label arrays retain insertion order in every oracle.

Ordinary tests copy these immutable inputs into temporary directories. They do
not build old code, download dependencies, or modify retained snapshots:

```sh
cargo test --locked --test storage_compatibility
```

The migration I/O fault sweeps also copy schemas 1–4 and use their JSON oracles.
They check raw schema/data rollback before retrying through the public opener:

```sh
cargo test --locked --lib database::storage_failures::migration_io
cargo test --locked --lib database::storage_failures::migration_read_io
```

The read sweeps include connection headers, schema/backfill reads, and commit's
journal probe. They also verify complete migrated contents when SQLite absorbs
a transient error in its internal schema-1 check. See the
[write/sync model](../../../docs/semantics.md#migration-io-failures) and
[read model](../../../docs/semantics.md#migration-read-failures) for boundaries
and limits.

To audit file hashes (Python 3.12+):

```sh
python3 tests/fixtures/compatibility/rebuild.py --verify
```

To author a separate comparison set, with full local Git history, Rust 1.99,
and the historical locked dependencies available:

```sh
python3 tests/fixtures/compatibility/rebuild.py --output /tmp/new-crabnet-fixtures --offline
```

The destination must not exist. Omit `--offline` only if dependency downloads
are needed. The helper archives each exact revision, adds only an example from
`producer.rs`, and compiles/runs it with `--locked`. It does not edit the old
engine, manifest, or lockfile. Version-specific statements enable public APIs
available in each writer. Generated entity IDs are random, so a newly authored
set is semantically comparable but will not have the same bytes/hashes.

Review provenance, schema markers, graph oracles, and hashes before retaining
new evidence. Keep existing inputs unchanged. Add tagged release writers as
separate fixtures even when their schema number is unchanged, extending the
test table and manifest instead of overwriting these baselines. Negative tests
make clearly identified synthetic copies for future-version and experimental
single-label-v2 refusal; those copies are not historical format evidence.
