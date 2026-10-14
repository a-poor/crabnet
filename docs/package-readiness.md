# First-release package and API review

Development review started 2026-10-14, after commit `6056f4d`. The chosen
release channels are crates.io plus a Git tag/source archive. This review does
not select or publish a release candidate.

## Package boundary

Cargo metadata names `crabnet` 0.1.0, Rust 2024 / Rust 1.99, with the repository,
README, docs.rs destination, searchable keywords and database category declared.
The package remains a library; no default executable or build script is added.
Both project license texts, the TCK license and NOTICE, pinned TCK
outcomes, examples and documentation remain in the source archive. No dependency
source archives are vendored into it. The archive is approximately 1 MiB
compressed, so this review does not remove reproducibility evidence for size.

[THIRD_PARTY.md](../THIRD_PARTY.md) records the vendored-material review and
links the [locked inventory](dependency-inventory.json): 98 third-party packages,
64 reachable through non-development dependency edges across all targets and
34 development-only packages. These are declarations and source-file hashes,
not a binary compliance certification. Three archives lack top-level license
files; the inventory records that explicitly. Cargo.lock remains unchanged.

## Public API review

| Surface | Reviewed contract |
| --- | --- |
| `Database`, opening options | Mutable execution, existing/read-only modes, connection-local lock timeout; no public raw SQLite connection. |
| `Parameters`, `Value`, `CypherValue` | JSON inputs and typed outputs are separate. Integer/float accessors do not coerce. Graph snapshots retain IDs, ordered paths, labels and properties. Derived Rust equality is not a substitute for every Cypher equality rule. |
| `QueryResult`, `QuerySummary` | Column order, duplicate rows, query-scoped counters, and provisional transaction results are documented. |
| `Transaction`, constraints | Commit/rollback ownership, whole-transaction abort on query errors, uniqueness behavior and ambiguous commit retry outcomes are documented. |
| Compiled/prepared reads, cursors | Owned unbound plans versus borrowing bound handles; snapshot lifetime, iterator errors, and incremental/buffered mode remain explicit. |
| Query options | Cooperative limits/deadlines/cancellation; no claim of a whole-process memory bound. |
| Errors | Non-exhaustive categories remain public. `Busy`/`Sqlite` expose rusqlite error types, and `Value` exposes serde_json's type; dependency changes must preserve these source contracts. |
| Backup/restore and schema | New backup paths, quiescent replacement, first-release storage baseline and no automatic downgrade. |

`OpenMode`, `OpenOptions`, `TraversalLimits`, and `QueryOptions` are exhaustive
public types today. Preserve their source compatibility in 0.1 patch releases;
do not casually add variants or required fields. Result/value structs, `Error`,
`CypherValue` and `ReadMode` already use `#[non_exhaustive]`. No new compatibility
break or public signature change was introduced by this review.

The strict rustdoc audit initially found 34 undocumented public items. Their
fields, variants, accessors and result alias are now documented, along with a
crate-level quickstart. `#![warn(missing_docs)]` makes future omissions visible
and the existing Clippy `-D warnings` gate rejects them without another CI job.

## Repeating the local package rehearsal

With cached Cargo dependencies, Python 3.12+ and the declared Rust toolchain:

```sh
python3 scripts/dependency-inventory.py --check
cargo rustdoc --locked --offline --lib -- -D warnings -D missing_docs
python3 scripts/check-package.py
```

On an uncommitted development tree, explicitly add `--allow-dirty` to the last
command. That is development evidence only. The script never publishes and
retains its temporary directory and `evidence.json` for inspection. It:

1. Builds and verifies the actual Cargo archive, checking required license,
   notice, inventory and example members before extraction.
2. Creates an independent consumer with a fresh build directory, depending on
   the unpacked crate rather than the checkout. It preserves locked dependency
   versions, compiles each Rust README example, and runs the public-API smoke
   client for typed values, constraints, commit/rollback, compiled rebinding,
   cursors, backup/restore, identity and read-only reopening.
3. Runs the unpacked crate's doctests, pinned TCK, current-format opening, backup
   and transaction integration tests without access to repository history.
4. Records the source revision/status, toolchain, archive and lockfile hashes,
   member count, README example count, and successful command exit statuses.

README examples requiring existing files are compiled, not executed as-is;
the smoke client executes equivalent opening/backup paths in a fresh directory.
The local review does not replace the candidate's complete Ubuntu checks.

## Results and outstanding release checks

The final development rehearsal passed on 2026-10-14 using Rust 1.99.0 on
`aarch64-apple-darwin`, with cached dependencies and no lockfile changes:

- Cargo built and verified the 427-file archive (1,013,485 compressed bytes).
- All eight packaged README Rust examples compiled in the independent consumer.
- The public-API smoke client passed from the unpacked crate.
- All 28 packaged tests/doctests passed: 13 doctests, five storage-compatibility
  tests, one pinned TCK verifier, and nine transaction tests.
- Strict rustdoc (`-D warnings -D missing_docs`), all-target Clippy with warnings
  denied, formatting, whitespace checks, and inventory verification passed.

The tested archive SHA-256 is
`7d14583980937c38f3547f2b71ed0b156ad4b903136daa6f260917f99b87a08f`.
Its lockfile SHA-256 is
`62ecc54c0470425e7555184d9883f8ae99395ec8445f79e3f36b4e1314c6e102`.
This was an explicitly dirty development archive based on `6056f4d`, built
before these result paragraphs were added. The retained rehearsal directory
contains the archive, extracted sources, independent consumer, and full
`evidence.json`. Rebuild from the final clean candidate rather than publishing
this development artifact.

The maintainer verified on 2026-10-14 that `crabnet` is available on crates.io.
Automated API lookups returned HTTP 403. Publishing credentials and registry
access still need verification; name availability does not establish either.
No credentials, tags, remote branches, or registry entries were changed.

The [draft release notes](release-notes-0.1.0.md) and [first-release gates](first-release.md)
retain the required Ubuntu candidate run, Git Graph acceptance, clean artifact hashes, and publication approval.

## Clean candidate rehearsal: 2026-10-14

`python3 scripts/check-package.py` passed from clean commit
`82a905ea9de0550c45cdddae8331fea6b4cd373d`, without `--allow-dirty`, on
Rust 1.99.0 / `aarch64-apple-darwin`. The rehearsal completed successfully
with the results below.

- Archive: 427 files, 1,005,564 compressed bytes.
- SHA-256: `d401ad4467b22482080b66702ef74a71b2b7b60fcc43840ceec9212af3b7fc55`.
- The rewritten README's one Rust example compiled against the unpacked crate.
- The independent public-API consumer passed.
- All 31 packaged checks passed: 16 doctests, five historical storage tests,
  one pinned TCK verifier, and nine transaction tests.
- The 98-package dependency inventory matched. An archive scan found no
  machine-specific home, checkout, or temporary directory prefixes.

This validates the archived source at that commit. This report was added
afterward and is not included in that archive. Repackaging a later source
revision produces a different artifact and requires a new check.
The candidate's [Ubuntu CI run](https://github.com/a-poor/crabnet/actions/runs/38594191484)
passed: 1,028 tests/doctests and 147 benchmark smoke cases, formatting, Clippy,
whitespace checks, and unchanged dependency/TCK pins.
The optional lifecycle job was skipped. Git Graph
acceptance, publishing access, and
publication approval remain open.

## Package after log cleanup: 2026-10-14

The clean `465c78995db12acf0f54264a7c5deb0883343613` archive passed
`python3 scripts/check-package.py`: one README example compiled, the independent
consumer passed, and all 31 packaged tests/doctests passed. Cargo's archived
VCS metadata confirms the exact revision and clean source. The final local
script status also sees later fixture/documentation work, which is outside
this already-built archive.

- Archive: 413 files, 900,190 compressed bytes.
- SHA-256: `af35ed2fcc14a78f13d28b92d3a7da8b784ebc2e4d52b8c964e57025f540a352`.
- Machine-path scan passed across all archived files.

Raw logs and generated reports remain local. Subsequent test cleanup and
documentation are not part of this archive; packaging must be
repeated if those changes are included in the published source.

## First-release fixture cleanup

Historical and candidate writer fixtures and their dependent tests were removed
because no published earlier release exists. Prior test/archive counts above
remain records of those earlier runs. The package rehearsal now checks opening,
backup and transactions alongside doctests and the TCK; rebuild the package
from the final source after this cleanup. No fixture-retention gate remains.
