# First release: 0.1.0

Status: preparation started 2026-10-13; no release candidate is qualified or
published. This is the working checklist for an **experimental 0.1.0 release**,
not a declaration that milestone 11 production qualification is complete.

## Who this release is for

Rust developers building local tools, rebuildable graph indexes, and controlled
internal workloads. Git Graph is the acceptance application. Users pin the
version, keep authoritative source data or verified backups, and handle the
documented errors and recovery procedures.

Ship the existing documented query/API surface, including transactions,
constraints, compiled reads, cursors, migrations, and backup/restore. Do not add
language features to reach an arbitrary completeness percentage. The
[semantics](semantics.md), [coverage](coverage.md), and pinned TCK outcomes define
the subset; unsupported behavior must be documented or rejected.

The initial release validation target is Ubuntu 24.04 x86-64, Rust 1.99.0,
locked bundled SQLite, and local DELETE-journal files with serially used
connections and one writer. Local macOS checks are useful development evidence,
but are not a substitute for fresh Ubuntu candidate evidence. See
[support](support.md) for exact configuration and resource-control limits.

## Release gates

Check an item only with linked evidence. Earlier successful runs are baselines,
not validation of a later candidate. Candidate code changes invalidate affected
evidence; record what was rerun and why.

| Gate | Required result | Status / evidence |
| --- | --- | --- |
| Scope and compatibility | Review the public API, query/error contracts, schema 5 and the pre-1.0 patch policy. List intentional deviations and limitations in release notes. | Public API, error/value contracts and patch boundaries reviewed in [package readiness](package-readiness.md); [draft release notes](release-notes-0.1.0.md) describe scope and limits. Final candidate review remains required. |
| Known correctness blockers | Fix or explicitly reject incorrect results in the chosen surface. Initial queue: integer-conversion overflow/precision, plus review of the six remaining TCK expected-error exclusions against documented intentional behavior. | NUL and conversion fixes pass complete local checks in the working tree. Six exclusions reviewed below; no TCK expectations changed. Candidate validation remains required. |
| Candidate checks | Run `bash scripts/check.sh` on the exact candidate on Ubuntu: all targets and 1k/10k benchmark smoke cases, doctests, unchanged TCK verification, formatting, Clippy, and diff checks. Record revision, lockfile, toolchain, runner and filesystem details, commands, and results in a concise report. | Passed for `82a905e` on Ubuntu 24.04 / Rust 1.99.0: 1,028 tests/doctests, 147 benchmark smoke cases, formatting, Clippy, whitespace and unchanged pins. [CI run](https://github.com/a-poor/crabnet/actions/runs/38594191484) passed. The optional lifecycle job was skipped. |
| Application acceptance | Git Graph pins the candidate in its isolated checkout. Pass correctness tests and the original 14 pilot gates, including 100 lifecycle cycles. Rerun the declared compare, containment, merge-base and history protocols against their existing thresholds; summarize results and historical failures. | Candidate `82a905e` passed 67 application Rust tests, 16 Python tests, all 14 pilot gates/100 cycles, containment (8/8), merge-base (10/10), and history (8/8). Compare passed 5/6 latency cases; 10k diverged p95 was 6,101.778 ms against a 6,000 ms gate. Follow-up operation-scoped compiled reads passed all six compare cases, with 10k diverged p95 2,303.285 ms under the unchanged gate; all 90 samples matched the Git oracle and work counts. The original failure remains recorded. |
| Failure and retry contract | Rerun existing allocation, one-shot I/O, migration, backup/restore, contention and process-recovery tests. Investigate journal deletion at commit specifically: record whether a reported failure can leave committed changes and what retry is safe. Resolve ambiguous public retry guidance before release. | Existing evidence is linked from [milestone 11](roadmap.md#11-production-qualification--in-progress); journal deletion investigation is complete locally: pre-delete errors recover the original state, while a modeled post-delete error leaves committed writes and blind retry can duplicate them. See [commit retry semantics](semantics.md#commit-io-failures). Candidate checks still repeat this evidence. |
| Independent semantic checks | Add a bounded deterministic conversion/value-boundary corpus with independent expected values. Record which contracts it checks; do not call it full differential Cypher qualification. | Four tests in [integer_conversion.rs](../tests/integer_conversion.rs) cover explicit binary64 neighbors, exact integer text, null/error behavior, compiled/cursor execution and writes; complete local checks pass. Broader reference-engine differential work stays in milestone 11. |
| Package readiness | Review name/version, description, repository/documentation metadata, licensing and third-party notices, dependency inventory, README examples, and archive contents. Build/test the packaged crate in a clean consumer project and verify the locked build. | Maintainer chose `MIT OR Apache-2.0` on 2026-10-13; both license texts and Cargo metadata are added. Initial offline metadata/file-list inspection is recorded below. Third-party inventory, public-doc audit and archive/clean-consumer rehearsal are recorded in [package readiness](package-readiness.md). Clean commit `82a905e` passed the [package rehearsal](package-readiness.md#clean-candidate-rehearsal-2026-10-14), including 31 packaged checks; the maintainer verified name availability on 2026-10-14; publishing credentials/access remain unverified. Repackage if the publication source changes. |
| Release record | Finalize release notes and a concise report linking checks, limitations, current-format recovery instructions, exact commits and artifact hashes. | Pending final source/artifact selection. No historical or candidate writer fixture is required for the first release. |
| Publication | Review final evidence for crates.io plus a Git tag/source archive, then explicitly authorize tagging/publishing. Verify the published artifact matches the qualified source. | Channels selected by the maintainer on 2026-10-14: crates.io plus Git tag/source archive. Publication is not authorized by this preparation task. |

## Boundaries and deferred work

This release does not claim full Cypher conformance, production qualification,
power-loss/torn-write durability, persistent/partial I/O recovery, arbitrary
external SQLite modification, WAL/network filesystem support, multi-writer
execution, or a whole-query/process memory cap. Existing buffering boundaries
remain documented. Acknowledging these limits does not excuse a known wrong
answer or data-integrity defect inside the chosen contract.

Mac/Windows qualification, temporal types, procedures, broader streaming,
additional indexes/optimizations without measurements, and Git Graph decorations
or other UI extensions are follow-ups, not first-release blockers. The stronger
[milestone 11](roadmap.md#11-production-qualification--in-progress) gates remain
open; this experimental release does not mark them complete or erase their
unqualified fault models.

Hosted checks must actually execute successfully before publication. Prior
Actions billing/usage failures are infrastructure failures, not passing tests.
Resolve them or arrange and document equivalent fresh Ubuntu validation.
If the repository plan still prevents required-check enforcement, record the
limitation and manually review the candidate's checks. Enforcement remains an
open production-qualification item.

## Preparation record

The first development slice passed `bash scripts/check.sh` locally on
2026-10-13 with Rust 1.99.0: **1,021 tests/doctests and 147 benchmark smoke
cases**, unchanged TCK pins, formatting, Clippy with warnings denied, and
`git diff --check`. Before implementation, three of the four new conversion
tests failed on the incorrect values described below. The focused/adjacent
function, conversion, numeric-error and property-index suites then passed all
29 tests. This validates the current working tree, not a named release candidate
or the required fresh Ubuntu run. No dependency lockfile or schema changed.

### Package inventory

Initial packaging inspection used `cargo metadata --locked --offline` and
`cargo package --list --allow-dirty --offline`. The initial resolved inventory
had 99 packages, each declaring a license. The initial 412-file package list included
both Crabnet license texts and the vendored TCK license, and emitted no warnings.
Those initial checks established inventory and inclusion only: they did not build the
archive, validate a clean consumer, audit notices, or establish publication
rights for third-party materials. The subsequent [package review](package-readiness.md)
records the 427-file archive build, eight README example compile checks,
independent consumer, and 28 passing packaged tests/doctests. Repeat the
rehearsal on the final clean candidate.

### Integer conversion and TCK deviations

The integer-conversion regression reproduced two wrong results: numeric `2^63`
clamped to `i64::MAX`, and the integer string `'-9223372036854775809'` rounded
through binary64 to `i64::MIN`. Both must return null. The fix checks a float's
truncated range before conversion and parses signed decimal integer text exactly,
without a float fallback after overflow. Decimal/exponent strings retain the
documented binary64 interpretation. Existing type errors and argument-evaluation
errors remain errors; null conversion results are not transaction failures.

The six `expected-error-not-raised` entries in the pinned
[TCK exclusions](../tests/tck/exclusions.txt) were reviewed individually against
their scenario queries and existing contracts on 2026-10-13:

| Pinned scenario | Crabnet behavior retained for 0.1.0 |
| --- | --- |
| `clauses/return/Return2.feature` [15], deleted-node property | Post-delete projection reads the materialized pre-delete node snapshot. |
| `clauses/return/Return2.feature` [16], deleted-node labels | Labels remain available from that same snapshot. |
| `clauses/return/Return2.feature` [17], deleted-relationship property | Relationship properties remain available from the pre-delete snapshot. |
| `clauses/set/Set1.feature` [10], list-of-maps property | Nested JSON lists/maps are intentionally storable property values. |
| `expressions/pattern/Pattern1.feature` [22], RETURN pattern | A supported fixed pattern is a boolean existence predicate in a projection. |
| `expressions/pattern/Pattern1.feature` [23], WITH pattern | The same boolean predicate can be projected through WITH. |

These are explicit compatibility deviations, not newly discovered correctness
bugs under Crabnet's contract. Preserve their exclusions and disclose them in
release notes. Existing [delete tests](../tests/delete_mutations.rs),
[property tests](../tests/property_invariants.rs),
[pattern tests](../tests/pattern_predicates.rs), and the pinned TCK verify the
relevant behavior. This review does not constitute review of every other TCK
exclusion or full Cypher conformance.

## Work order and completion rule

1. **Complete locally:** integer-conversion boundary fix and review of the six
   expected-error exclusions, without expanding the language surface. Candidate
   checks still repeat this evidence on the release revision.
2. **Complete locally:** [journal deletion fault tests](../src/database/storage_failures/commit_delete.rs)
   and public retry guidance.
   A reported commit error can accompany persisted writes; the documented model
   distinguishes recovered rollback from post-deletion commit and proves the
   duplicate-write risk of blind retry. Local validation passes: 1,024
   tests/doctests, 147 benchmark smoke cases, formatting, and Clippy. The six
   focused commit tests also pass after the final test-helper lint cleanup.
   Candidate checks repeat this evidence.
3. **Complete locally:** [package/API/license review](package-readiness.md),
   reproducible offline package rehearsal, and [draft release notes](release-notes-0.1.0.md).
   The selected distribution is crates.io plus a Git tag/source archive.
   The maintainer verified name availability on 2026-10-14. Publishing access
   and final artifact checks remain pending.
4. **Next:** select a candidate commit, run Ubuntu checks, and coordinate Git Graph's
   candidate workload qualification.
5. Complete the release report and request final
   publication approval. Any failed gate stays visible and blocks publication
   until resolved or the release scope is explicitly revised with justification.

Each work session should update the relevant row and link its concrete result.
Do not keep extending the checklist merely because another feature or speculative
test is possible. Add a blocker only for an observed defect, a concrete integrity
risk, or evidence needed for a claim this release makes.

### Compare acceptance follow-up

Git Graph's initial candidate run remains a failure: the 10k diverged comparison
recorded 6,101.778 ms p95 against its unchanged 6,000 ms limit. Correctness,
query/work counts, and memory limits passed. No sample is excluded or rescored.

Inspection found compare performs its two repeated Cypher reads through ordinary
execution, while merge-base/history already use operation-scoped compiled reads.
The application fix reuses the existing compiled-read helper with
identical query text (including parent ordering), fresh parameters and controls,
and unchanged work accounting. Compilation stays inside the operation's latency
and deadline. It does not require an engine or public API change. All 67 application Rust tests, formatting, Clippy, doc checks and the release
build passed. The complete six-case compare protocol passed all 90 samples:
the formerly failing case recorded 2,303.285 ms p95 and maximum process RSS was
14.33 MiB across cases. Every Git oracle and exact work counter matched.
These are separate shared-host runs, not a controlled speedup measurement.
The compare latency blocker is resolved without an engine change; the
application patch and final publication source still need to be committed.

The docs-only cleanup candidate `465c789` also passed
[Ubuntu CI](https://github.com/a-poor/crabnet/actions/runs/38597683794).
Its engine and lockfile match `82a905e`. Subsequent test/documentation cleanup
requires fresh candidate checks.

### First-release compatibility scope

There is no backwards-compatibility requirement for unreleased development
commits. Historical/candidate SQLite fixtures, their generation tooling, and
the fixture-dependent migration tests have been removed. Current-format
opening, backup/restore, recovery and invalid-file tests remain. The earlier
Git Graph revision rollback rehearsal is development history, not a publication
gate. Future published-version upgrades can add targeted fixtures when needed.
