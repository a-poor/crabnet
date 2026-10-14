# Architecture and maintenance

Crabnet combines parameterized SQLite execution with Rust graph traversal,
typed value comparison, row shaping and hydration. Its public API is exported
from `src/lib.rs`; private module boundaries are implementation details.

```text
Cypher -> parser -> logical lowering/type checks -> SQLite/physical plan
       -> execution in one snapshot/transaction -> typed rows + summary
```

## Code ownership

| Area | Responsibility |
| --- | --- |
| `src/planner/lower.rs`, `logical.rs`, `typecheck.rs` | Bind names and kinds; lower clauses and scopes; reject unsupported shapes. |
| `src/planner/sqlite.rs`, `nested.rs` | Generate parameterized SQL and graph-value transport expressions. |
| `src/planner/physical.rs`, `explain.rs` | Prepare SQL/Rust operator composition and inspectable plans. |
| `src/database.rs` | Database entry points, execution orchestration, mutation materialization, pending rows, hydration and schema initialization. |
| `src/database/functions.rs`, `text.rs`, `nested.rs` | Register private SQLite functions for typed values, numeric/text semantics and nested graph transport. |
| `src/database/comparison.rs` | Typed equality, order and semantic keys used by row operations. |
| `src/database/compiled.rs`, `cursor/`, `cursor.rs`, `transaction.rs` | Reusable reads, cursor lifecycle, snapshots and transaction ownership. |
| `src/database/traversal.rs`, `resources.rs` | Graph producers, budgets, cancellation and deadlines. |
| `src/database/open.rs`, `backup.rs`, `constraints.rs` | File opening/contention, backup/restore and uniqueness maintenance. |
| `src/database/tests.rs`, `storage_failures/` | Internal regressions and controlled storage fault models; no public fault-injection API. |
| `src/value.rs`, `error.rs`, `query.rs` | Public values, structured errors and query controls/results. |

`database.rs` still owns schema constants and the persisted `semantic_json_key`
encoding. The encoding is shared by functions and uniqueness maintenance;
changing it requires migration analysis. Moving a helper must not accidentally
change its bytes, ordering, error mapping or connection-registration sequence.

## Boundaries to preserve

- Bind user data; only compiler-owned structure becomes SQL text.
- Preserve row multiplicity and typed graph identity across every transport.
- Prepare the entire read before execution, including deferred parameters and
  runtime producers. Keep execution controls fresh on each compiled invocation.
- Keep mutation input stable and transactions atomic. Cursor drop/error/exhaustion
  must release the appropriate snapshot and resources.
- Treat streaming versus buffering as an explicit supported-shape contract.
- Keep current-format opening, backup/restore, and refusal tests aligned with
  storage behavior. Add released-format migration tests when an upgrade is needed.

See [semantics](semantics.md) for the actual contracts and the
[test catalog](test-catalog.md) for their checks.

## Cleanup policy and follow-ups

The 2026-10-13 cleanup separated SQLite function registration and internal tests
from the execution module, moved historical milestone detail out of the active
roadmap, and separated the test catalog from the testing workflow. Public APIs,
schema/semantic-key encoding, SQL function names, and test identities are retained.

The logical lowerer and SQLite compiler are still large. Split them only around
a coherent responsibility with explicit dependencies, not a line-count target.
Mutation execution, schema lifecycle and hydration are further candidates inside
`database.rs`; move them separately when their ownership becomes clearer. Avoid
introducing a generic abstraction solely to share a few test assertions. Local
fixtures can be preferable when they make different semantic expectations clear.

For structural changes, compare moved bodies, run the affected existing suites,
then the [complete checks](testing.md#focused-commands). Document new contracts
only when behavior intentionally changes. Cleanup should preserve the release
checklist and make its next steps easier to find, not add new release scope.
