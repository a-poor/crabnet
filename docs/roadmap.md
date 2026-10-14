# Crabnet roadmap

This is a living plan for an experimental embedded property-graph database. It
is intentionally a direction of travel rather than a compatibility promise.
Update it when implementation work teaches us something better.

The core engine and application foundations are implemented for a substantial,
documented subset. The Git Graph downstream Rust application pilot completed
milestone 10 for its recorded workload and configuration. Crabnet is usable for
local tools, rebuildable indexes, and controlled application workloads within
that documented scope. Near-term work prioritizes known correctness problems
and concrete application needs. **Production qualification for a defined
supported configuration** remains an explicit workstream (milestone 11), with
open release gates. Further language features, optimization, and fault coverage
are selected by application evidence, data-integrity risk, and the claims a
release needs to support.

## Project posture

- Git Graph is a downstream Rust consumer pinned to a validated Crabnet baseline.
  The implementation and public contracts remain experimental; coordinate changes
  affecting that consumer with an explicit upgrade or rebuild path.
- Schema compatibility remains experimental. Version 5 preserves the upgrades
  from main's versions 1–4; migrations and index backfills run atomically.
  Unreleased branch-only formats may require recreating the database.
- A pilot pins its Crabnet version and records its API and storage assumptions.
  Changes affecting an adopted pilot need an explicit upgrade or rebuild path.
  Completing the pilot does not itself establish a stable public API or storage
  compatibility promise; milestone 11 defines those commitments.
- Use repository history to recover earlier code and the documented backup and
  recovery workflow for database files. Keep code clear enough to support the
  next experiment.
- Correct results or a structured plan-time rejection are both acceptable.
  Plausible but incorrect results are not.

## Where we are

The implemented baseline includes:

- openCypher text is parsed by the in-house parser, bound, and lowered into a
  slot-based logical row plan;
- fixed-length node/relationship patterns compile to parameterized SQLite;
- the physical schema uses `nodes` and `edges`, while Cypher-facing code uses
  the word *relationship*;
- computed scalar projections, typed JSON properties, comparisons, membership,
  string predicates, arithmetic, semantic distinctness, ordering, pagination,
  and named parameters cover a useful read subset;
- read-only queries can cross standalone or chained `WITH` boundaries, carrying
  explicitly projected entity, path, and scalar bindings into later clauses
  while enforcing scope loss;
- owned compiled reads amortize parsing/lowering/SQL generation across changing
  parameters, with eager Database/Transaction execution, fresh per-call controls,
  coherent snapshots, and explicit connection/schema compatibility;
- aggregation/grouping, list expansion, query unions, and fixed-length optional
  matching compose through that read pipeline with explicit bag, type, and
  null-extension semantics;
- legacy variable-length traversal uses recursive SQLite CTEs for explicit
  maxima through 32 and Rust relationship-unique trail enumeration for larger
  or omitted maxima. Eligible cursors stream encoded singleton trails; eager
  execution and buffered cursor shapes materialize runtime JSON relations;
- mandatory read-only fixed, variable-length, and mixed patterns can bind public
  typed paths, including zero-hop values; SQLite carries ordered IDs into
  pending cells, which Rust shapes before query-wide hydration in eager reads
  or bounded row batches in eligible cursors, on a shared snapshot;
- fresh legacy relationship variables, including deep/open-ended forms, bind
  traversal-ordered typed relationship lists with zero-hop `[]` through the same
  identity shaping and hydration pipeline;
- legacy `shortestPath` and `allShortestPaths` factors use Rust BFS over one
  variable-length relationship to select one deterministic but unspecified
  minimum trail or all tied unweighted minimum trails per endpoint pair. Lower
  bound zero includes identity; lower bound one permits shortest positive
  same-endpoint cycles. Results reuse the path/list composition and hydration
  boundary;
- relationship uniqueness spans every constituent path pattern in one `MATCH`,
  while a later `MATCH` starts an independent scope;
- `CREATE` and `MATCH ... CREATE` run transactionally, preserve input-row
  multiplicity, and roll back atomically;
- matched node and relationship properties support direct `SET`/`REMOVE` plus
  literal/parameter whole-map replacement and merge;
- matched `DELETE` and `DETACH DELETE` deduplicate targets across the input bag,
  report actual storage-row counters, and roll back atomically;
- `MERGE`, application-managed node uniqueness constraints, and transactions
  spanning multiple queries support import/update workflows with read-your-writes
  and rollback on query failure;
- prepared reads expose incremental or buffered execution before starting.
  Eligible single-trail cursors compose through `WITH` projections, filters,
  DISTINCT and pagination, chained `UNWIND`, final DISTINCT/UNION, and batched
  hydration. Constrained starts load neighbors on demand. The corresponding
  implementation entries below record each slice and its validation;
- query options bound traversal work and retained traversal state, SQL work,
  transfers, and results, with cooperative cancellation and deadlines. These
  counters do not constitute a whole-query memory cap;
- file opening modes, lock contention, atomic migrations, validated backup and
  restore, and process-termination recovery have local-file coverage; and
- structured errors distinguish planning, type/data, constraint, resource,
  cancellation, and contention failures.

The executor combines parameterized SQLite fragments with Rust traversal,
semantic row shaping, and hydration. Incremental reads retain their snapshot
until exhaustion, error, or drop; eager and ineligible cursor shapes buffer.
Global traversal sorting/aggregation, additional joins, multiple producers,
and shortest-path output still have buffering boundaries. SQLite can also
materialize internally. The supported concurrency contract is independent,
serially used connections on local files with DELETE rollback journaling and
one writer at a time. Long-lived cursors can block commits.

The dependency-graph example exercises a full persistent lifecycle and has
recorded measurements at 1,000 and 10,000 packages. The downstream
[Git Graph pilot](git-graph-pilot.md) passed all 14 acceptance gates, including
100 update/import/query/reopen cycles, at the recorded engine baseline. These
supply application, regression, and benchmark evidence. API/schema stability,
a qualified platform matrix, and power-loss/disk-I/O failure behavior remain
explicit work.
See [coverage.md](coverage.md), [semantics.md](semantics.md), and
[cursors.md](cursors.md) for the current supported surface and limits.

<a id="near-term-working-order"></a>

## Active work

Follow the [first-release checklist](first-release.md) for the bounded 0.1.0
work order, acceptance gates, completed preparation, and decisions. Next is the
commit journal-deletion/retry investigation. Fix observed wrong answers and
integrity risks in the documented surface before adding features. Git Graph
provides the application acceptance workload.

<a id="milestones"></a>

## Milestone status

Detailed implementation notes and historical restrictions are retained in
[the milestone archive](history/milestones.md). This page describes current
status; the archive explains how we got here.

### 1. Make the current contract explicit

Current contracts live in [semantics](semantics.md), [coverage](coverage.md), and
the reviewed test corpus. Keep them synchronized with behavior changes.

### 2. Introduce real Cypher result values — complete

Typed scalars, collections, nodes and relationships are implemented. Paths and
nested graph values are also supported; see the current semantics.

### 3. Finish the fixed-length read core — complete

Fixed-pattern reads, typed predicates, ordering, DISTINCT and pagination are
implemented with bag semantics and structured errors.

### 4. Model clauses and scopes as a pipeline — complete

Clause pipelines preserve scope across WITH, aggregation and mutation stages.
Use current coverage rather than the original milestone restrictions.

### 5. Add core matched mutations — complete

CREATE, SET/REMOVE, DELETE/DETACH DELETE and MERGE are implemented with rollback
and mutation summaries. Supported expression targets are documented.

### 6. Add query composition — complete

Aggregation, grouping, UNWIND, UNION and optional matching compose through the
read pipeline. Supported cursor shapes and buffering remain explicit.

### 7. Add bounded variable-length traversal — complete

Bounded and open-ended trails, typed paths and relationship lists, and unweighted
shortest-path forms are implemented with declared resource limits.

### 8. Generalize hybrid execution — complete

SQLite fragments and Rust operators share preparation, typed row shaping,
hydration and query controls. Eligible cursors stream; other shapes buffer.

### 9. Conformance and optimization — in progress

Conformance and measured optimization are ongoing. The pinned TCK currently
records 2,613 passing and 1,269 excluded scenarios, no result-mismatch rows, and
six expected-error-not-raised cases. Those six are documented intentional
deviations reviewed in [first-release preparation](first-release.md#integer-conversion-and-tck-deviations).
Remaining exclusions include unsupported features and error-category differences;
this is not full Cypher conformance. Select new work from concrete bugs or
application measurements, not the number of unchecked language features.

<a id="10-application-usability--in-progress"></a>

### 10. Application usability — complete (2026-10-09)

Git Graph passed all 14 pilot gates on its recorded engine/host configuration,
including 100 update/query/reopen cycles and 1,000/10,000-commit workloads.
Subsequent application features have their own evidence. See the
[pilot report](git-graph-pilot.md); application success does not qualify a later
engine revision or establish production support.

#### Pilot completion checklist

The completed checklist and per-feature evidence are retained in the
[historical pilot checklist](history/milestones.md#pilot-completion-checklist).

<a id="11-production-qualification--planned"></a>

### 11. Production qualification — in progress

Production qualification remains open. The experimental 0.1.0 plan in
[first-release.md](first-release.md) is a bounded preparation track and does not
mark these stronger gates complete. See the checklist below.

| Production gate | Current status |
| --- | --- |
| Supported configuration | Defined in [support.md](support.md); definition alone is not qualification. |
| Compatibility policy and historical baselines | Defined and retained in [compatibility.md](compatibility.md); named release-pair evidence is still required when those releases exist. |
| Automated release checks | Earlier hosted matrix passed; current CI targets Ubuntu/Rust 1.99.0. Required-check enforcement remains blocked by the repository plan. Fresh candidate validation is required. |
| Failure behavior and durability claims | Extensive allocation, one-shot I/O, migration, backup/restore and process-recovery tests exist. Commit journal-deletion tests now distinguish recovered rollback from persisted writes after an error and document reconciliation before retry. Persistent/partial I/O, filesystem ENOSPC, journal truncation, and power-loss models remain unqualified. See [support.md](support.md) and the linked semantic contracts for tested boundaries. |
| Correctness of the declared surface | Recent numeric, string, aggregate and conversion fixes are covered. Continue resolving observed defects; broader pinned-reference differential testing remains open. |
| Reproducible qualification record | Candidate workload/compatibility/recovery evidence, release notes, package review and final report remain pending. |

Existing fault-test details and their provenance are preserved in the
[production milestone history](history/milestones.md#11-production-qualification--in-progress).
A named release must meet every claim it makes; unqualified configurations and
failure models remain explicit.

## Feature follow-ups

These are not automatic first-release requirements. Promote one only for a
concrete application need, measured bottleneck, or defect:

- Additional streaming shapes: sorting/aggregation, joins, multiple producers,
  shortest-path output and unconstrained graph-input loading.
- Broader graph-value functions/ordering and mutation expression targets.
- Temporal types, procedures and additional Cypher compatibility.
- Composite/relationship constraints, Cypher schema DDL and new storage modes.
- Performance changes supported by a profile and retained before/after evidence.
- New platform qualification and stronger durability commitments.

Do not infer a deadline or release blocker from missing coverage alone. Keep
new work tied to the [release checklist](first-release.md) or a concrete
application requirement.

<a id="design-principles"></a>
<a id="target-pipeline"></a>
<a id="implemented-foundations"></a>
<a id="milestone-9-implementation-notes"></a>

## Archived detail

Older section links land here so existing references remain usable. The
[design principles](history/milestones.md#design-principles),
[target pipeline](history/milestones.md#target-pipeline),
[implemented foundations](history/milestones.md#implemented-foundations), and
[milestone 9 notes](history/milestones.md#milestone-9-implementation-notes)
are retained in the archive. Current code ownership is described in
[architecture.md](architecture.md).
