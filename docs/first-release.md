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
| Scope and compatibility | Review the public API, query/error contracts, schema 5, supported schema 1–4 upgrades, and pre-1.0 patch policy. List intentional deviations and limitations in release notes. | Scope proposed here; policy exists in [compatibility](compatibility.md); final review pending. |
| Known correctness blockers | Fix or explicitly reject incorrect results in the chosen surface. Initial queue: integer-conversion overflow/precision, plus review of the six remaining TCK expected-error exclusions against documented intentional behavior. | NUL and conversion fixes pass complete local checks in the working tree. Six exclusions reviewed below; no TCK expectations changed. Candidate validation remains required. |
| Candidate checks | Run `bash scripts/check.sh` on the exact candidate on Ubuntu: all targets and 1k/10k benchmark smoke cases, doctests, unchanged TCK verification, formatting, Clippy, and diff checks. Retain revision, lockfile, toolchain, runner and filesystem details, commands, exit statuses and logs. | Pending. Current local checks are development evidence. |
| Application acceptance | Git Graph pins the candidate in its isolated checkout. Pass correctness tests and the original 14 pilot gates, including 100 lifecycle cycles. Rerun the declared compare, containment, merge-base and history protocols against their existing thresholds; retain raw results and historical failures. | Pending candidate. Terminal rendering correctness is included; additional UI features are not required. |
| Upgrade and rollback rehearsal | Use historical writer fixtures and Git Graph's previous pinned engine to create/verify a backup; open an upgraded copy with the candidate; verify identities, typed bags, constraints and application results; back up/restore the new copy; run the old application against a copy of the untouched old backup. Record writes lost by rollback and rebuild assumptions. | Historical fixture tests exist; candidate application rehearsal pending. There are no earlier named Crabnet releases, so do not claim release-to-release qualification. |
| Failure and retry contract | Rerun existing allocation, one-shot I/O, migration, backup/restore, contention and process-recovery tests. Investigate journal deletion at commit specifically: record whether a reported failure can leave committed changes and what retry is safe. Resolve ambiguous public retry guidance before release. | Existing evidence is linked from [milestone 11](roadmap.md#11-production-qualification--in-progress); journal deletion investigation pending. |
| Independent semantic checks | Add a bounded deterministic conversion/value-boundary corpus with independent expected values. Record which contracts it checks; do not call it full differential Cypher qualification. | Four tests in [integer_conversion.rs](../tests/integer_conversion.rs) cover explicit binary64 neighbors, exact integer text, null/error behavior, compiled/cursor execution and writes; complete local checks pass. Broader reference-engine differential work stays in milestone 11. |
| Package readiness | Review name/version, description, repository/documentation metadata, licensing and third-party notices, dependency inventory, README examples, and archive contents. Build/test the packaged crate in a clean consumer project and verify the locked build. | Maintainer chose `MIT OR Apache-2.0` on 2026-10-13; both license texts and Cargo metadata are added. Initial offline metadata/file-list inspection is recorded below. Third-party/package review and clean consumer validation remain pending. |
| Release record | Write release notes and a candidate report linking every gate, known limitations, upgrade/recovery instructions, exact commits and artifact hashes. Retain a writer fixture from the final source for future patch compatibility checks; after tagging, append the tag association without replacing historical artifacts. | Pending. |
| Publication | Review final evidence, select the distribution channel (tag/source archive and/or crates.io), then explicitly authorize tagging/publishing. Verify the published artifact matches the qualified source. | Not authorized by this preparation task. |

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
`cargo package --list --allow-dirty --offline`. The current resolved inventory
has 99 packages, each declaring a license. The 412-file package list includes
both Crabnet license texts and the vendored TCK license, and emits no warnings.
These checks establish inventory and inclusion only: they do not build the
archive, validate a clean consumer, audit notices, or establish publication
rights for third-party materials. Repeat them on the final candidate.

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
2. **Next:** investigate commit journal deletion and make retry guidance match observed
   outcomes under the documented fault model.
3. Review package/API/license readiness and assemble draft release notes. Resolve
   the maintainer's distribution choice before packaging for release. The license
   choice is recorded above.
4. Select a candidate commit, run Ubuntu checks, and coordinate Git Graph's
   candidate upgrade, rollback and workload qualification.
5. Complete the evidence report, retain the writer fixture, and request final
   publication approval. Any failed gate stays visible and blocks publication
   until resolved or the release scope is explicitly revised with justification.

Each work session should update the relevant row and link its concrete result.
Do not keep extending the checklist merely because another feature or speculative
test is possible. Add a blocker only for an observed defect, a concrete integrity
risk, or evidence needed for a claim this release makes.
