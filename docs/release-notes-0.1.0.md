# Crabnet 0.1.0 — draft release notes

Unreleased. Target distribution: crates.io plus a Git tag/source archive.
No candidate is qualified and no publication is authorized yet. This draft
must be finalized with the exact revision and candidate evidence before release.

## What is included

Crabnet is an experimental Rust 2024 embedded property-graph library backed by
bundled SQLite. It targets local tools, rebuildable indexes, and controlled
workloads. Git Graph is the acceptance application. Rust 1.99 is the minimum;
initial candidate qualification targets Ubuntu 24.04 x86-64 and local
DELETE-journal storage, with one writer and serial use of each connection.

The release includes the documented Cypher subset, typed scalar/graph/nested
results, parameterized reads and mutations, explicit transactions, node-property
uniqueness constraints, reusable compiled reads, read cursors with explicit
buffered fallback, cooperative query controls, and backup/restore. The
[coverage](coverage.md), [semantics](semantics.md), and [cursor contracts](cursors.md)
define the exact surface; this is not full openCypher conformance.

## Storage and compatibility

Schema 5 is current. Valid main schemas 1–4 upgrade on writable opening;
read-only opening and restore sources require the current schema. Keep a
verified pre-upgrade backup and the old application until acceptance. There
is no automatic downgrade; rolling back through an older backup loses later
writes unless the application separately replays them. Follow the
[upgrade and rollback procedure](compatibility.md#upgrade-and-application-rollback).
There are no earlier named Crabnet releases; historical writer fixtures are
regression evidence, not release-to-release qualification.

Within the 0.1 minor line, patch releases preserve the documented Rust source,
query, value and error-category contracts and schema 5. Exact error messages,
SQL, plans and opaque ID formats are not stable interfaces. Raising the minimum
Rust version or breaking these contracts requires a minor release under the
[compatibility policy](compatibility.md).

## Corrections made during preparation

- Strings containing NUL retain characters after the NUL in length, slicing,
  replacement, and comparison operations.
- Integer conversion rejects out-of-range results with null rather than
  clamping or rounding overflowing integer text through binary64.
- Invalid negative string bounds produce the documented argument error while
  preserving null/type precedence and lazy branch evaluation.
- Commit retry guidance now explicitly distinguishes an error from rollback:
  journal deletion can succeed before an error is reported, leaving writes
  committed. Reconcile the stored outcome before retrying a non-idempotent
  operation. See the bounded [commit fault models](semantics.md#commit-io-failures).

## Intentional differences and limits

The six reviewed TCK expected-error exclusions remain intentional: deleted
entities can expose their captured property/label snapshots, nested lists/maps
are supported property values, and supported fixed pattern predicates may be
projected as booleans. The [first-release checklist](first-release.md#integer-conversion-and-tck-deviations)
identifies each scenario. Other exclusions remain pinned and visible; no new
conformance claim follows from this review.

Temporal types, procedures and portions of the Cypher language remain unsupported.
Some reads buffer; resource accounting is not a whole-process memory cap.
WAL, network filesystems, multi-writer operation, arbitrary external SQLite
modification, persistent/partial I/O, power-loss recovery and corruption repair
are not qualified. Review [support](support.md) before adopting the library.

Crabnet's code is `MIT OR Apache-2.0`. Vendored TCK material and dependencies
retain their own licenses and notices; see [third-party materials](../THIRD_PARTY.md).

## Still required before publication

Select a clean candidate; qualify it on Ubuntu; complete Git Graph acceptance
and upgrade/rollback rehearsal; retain the final writer fixture and artifact
hashes; verify crates.io name/ownership and registry checks; finalize these
notes and explicitly approve publication. See the [release gates](first-release.md#release-gates).
