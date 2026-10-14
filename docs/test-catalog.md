# Test catalog

Use the narrowest layer that can prove the behavior, while retaining an
end-to-end result test for user-visible semantics.

- `tests/engine.rs` is the broad end-to-end acceptance layer. It exercises
  Cypher text through the public `Database` API, including planner errors,
  matched writes, rollback, and resulting graph state.
- `tests/transactions.rs` exercises the public multi-query transaction API:
  read-your-writes, parameterized application decisions, commit and rollback,
  drop/panic cleanup, query errors aborting all prior work, graph and index
  restoration, per-query summaries and traversal budgets, hybrid hydration,
  write-lock release, and persistence after reopening. Transaction unit tests
  cover cleanup after SQLite itself has already rolled back and after a
  failed commit.
- `tests/compiled_reads.rs` covers owned plans and changing bindings, independent
  connections and threads, all supported historical opening migrations,
  read-only/reopen/restore/constraint changes, full-plan preflight and pagination,
  typed bags/paths/unions/distinct/hydration, fresh controls, transaction rollback,
  and immediate lock release. Compiled-read unit tests attempt concurrent writes
  during both UNION branches before hydration and cancel execution after an
  earlier branch has produced rows. The shared `repeated_reads` example/benchmark
  fixture verifies changing-OID results and records planning amortization under
  the [declared measurement protocol](compiled-reads.md).
- `tests/transaction_cursors.rs` covers reads of earlier writes in both cursor
  modes, typed bags/parameters, DISTINCT/UNION/pagination and hydration, prepared
  reuse with fresh budgets, successful cleanup preserving the parent, rejection
  of writes, errors during preparation/start/iteration, rollback of graph/index/
  constraint changes, cancellation/deadlines and transfer/output/SQLite limits,
  provisional rows after rollback, immediate lock release with failed handles
  retained, and consumer versus parent panic unwinding. Rustdoc examples cover
  transactional read/decide/write and enforce exclusive borrowing against writes
  and commit with compile-fail cases.
- `tests/opening.rs` covers file access modes, missing/empty/foreign/future
  files, migration policy, read-only hybrid reads and rejected writes,
  connection-local lock timeouts, concurrent openers, reader visibility,
  competing writers, and connection reuse. Raw SQLite connections are used
  only as lock/legacy-schema fixtures, including a reader held across a
  Crabnet commit to verify rollback of provisional graph/constraint changes.
  The timeout test uses a loose lower bound; other contention cases assert
  outcomes rather than timing. Threaded waiting uses a readiness channel and
  a short delayed release; concurrent opening uses a start barrier.
- `tests/backup.rs` covers backup publication without overwrite, memory/file
  and read-only round trips, preserved identities and relationship bags,
  constraint/index persistence, replacement/observer behavior, invalid and
  corrupted sources, lock failures, and connection/staging cleanup. Corruption
  fixtures use raw SQLite to violate foreign keys and persisted CHECK clauses;
  restore must reject them before touching the destination.
- `tests/storage_compatibility.rs` uses immutable SQLite files from pinned
  historical writers and independent JSON graph oracles. It checks all main
  formats 1–5 for upgrades, typed data/identity and bags, maintained indexes and
  constraints, failed-migration rollback/retry, refusal without mutation, and
  backup restoration while retaining the original rollback source. Ordinary
  tests use temporary copies and never rebuild historical code. Fixture
  provenance, hash verification, and authoring instructions live in the
  [fixture README](../tests/fixtures/compatibility/README.md). Add tagged release
  writers before claiming release-to-release qualification.
- `tests/recovery.rs` launches its own test binary as a controlled child writer.
  A readiness message follows either a spilled uncommitted transaction or a
  completed commit. The parent checks the journal/main-file evidence before
  killing and reaping the child, then checks recovery, retained commits,
  constraints, derived indexes, integrity, connection reuse, and backup/restore.
  A timeout and child guard prevent leaked processes on failed assertions.
  Both this helper and the `recovery_io` helper run the child harness with one
  test thread and frame readiness markers on their own lines, so exact marker
  matching also works after libtest's unterminated `test name ... ` prefix.
  This exercises process death without Rust cleanup, not machine power loss.
- `src/database/storage_failures.rs` is a test-only child module so it can cap
  the private SQLite connection with `max_page_count`; no raw connection or
  fault-control API is exposed to consumers. Two tests cover four standalone
  node/relationship insert/update cases and three transaction cleanup paths.
  The payload exceeds the entire capped file size, while headroom allows prior
  mutations. Update-hook observations prevent vacuous rollback assertions.
  Tests compare typed graph bags and exact label/property/uniqueness-index rows,
  preserve the `SQLITE_FULL` code, verify constraint restoration, immediate lock
  release with a live aborted handle, integrity/reopen checks, and successful
  writes after lifting the limit. This is a real allocation failure, with
  working rollback I/O; it does not exhaust the disk.
  Its `commit_io` child module adds two commit fault sweeps and a database
  isolation/one-shot check. `io_vfs` forwards native I/O and injects exactly one
  `SQLITE_IOERR_WRITE` or `SQLITE_IOERR_FSYNC` before a chosen call. Each sweep
  derives fault points from a healthy commit trace, verifies the exact successful
  prefix and failed event, and covers journal/database writes and syncs, including
  failure after earlier database page writes. Faults are armed only after
  successful transaction queries or from a standalone write's commit hook.
  Exact rollback, lock release before connection reuse, integrity, reopening,
  and successful retry are checked for every point. The VFS is never made the
  default; registrations and fault state are scoped to temporary database paths.
  This model uses DELETE journaling, FULL synchronization, no cache spill, and
  healthy rollback I/O. Migration, restore, and backup copying use the same
  model in the sweeps below.
  The `commit_delete` child adds three tests for journal finalization. Pre-delete
  faults (FULL/EXTRA) retain the journal and recover the original state;
  modeled post-delete errors (EXTRA) leave committed writes. Both standalone
  and explicit transactions verify healthy trace prefixes, exact extended
  errors, journal presence, autocommit, observer-first/original-first/close-first
  recovery, typed graph/index/constraint state, locks, integrity, reopen and
  retry after rollback. A separate path-isolation/one-shot check proves that
  blind retry can duplicate a committed CREATE. Native deletion succeeds before
  the injected post-delete error; no actual directory-sync or power failure is
  simulated. Persistent/partial I/O errors, OS ENOSPC, and journal truncation
  remain follow-ups.
  The `migration_io` child adds two sweeps over retained schema 1–4 files,
  arming the same VFS before initialization. It covers both a retained failed
  connection and the owning constructor's cleanup. A commit-hook marker proves
  statement and commit coverage. Before any retry, raw SQLite checks the old
  application/version markers, schema cookie, every schema object, and every
  table's typed row bag, plus integrity, foreign keys, and writer lock release.
  Public retry and read-only reopen must match the independent historical JSON
  graph oracle; subsequent index/constraint writes must persist. Fixture files
  are copied, never modified.
  The `migration_read_io` child adds two read sweeps over the same historical
  inputs: retained initialization with a cold one-page cache and spill disabled,
  and both writable public open modes armed before connecting. Every healthy
  database/journal read is faulted by byte range and occurrence, including
  repeated backfill reads and commit's journal-end probe. Exact prefixes and
  connection/commit markers verify the targets. Reported errors must preserve
  `SQLITE_IOERR_READ`, original file bytes and raw schema/typed rows, autocommit,
  handle/lock release, and public retry. SQLite absorbs some schema-1 internal
  `quick_check` read diagnostics; those outcomes are pinned separately and must
  match the healthy migrated storage before reopening. Independent graph
  oracles, integrity, constraints, and subsequent writes verify both paths.
  These sweeps share the source-cache guard and never modify retained fixtures.
  The tiny-cache sweep runs alone in a child test process so other connections'
  shared cache budgets cannot eliminate its repeated-read fault points.
  The `initialization_io` child adds three tests. Public opening sweeps missing
  paths, zero-byte files, and empty SQLite files; retained-connection sweeps
  cover both existing empty forms. Every healthy database/journal read, write,
  and sync is failed before the native call, with connection/commit markers
  and exact prefix/target checks. Tests preserve extended errors, original bytes
  and uninitialized metadata, verify no partial schema or sidecars, and acquire
  an exclusive writer lock before closing retained failed connections. A failed
  new-file open leaves an empty file in this model. Public retry on that path
  must match healthy initialized storage and support graph/index/constraint
  writes, backup/restore, and reopening. The third test checks path isolation
  and retries a final-sync failure through the same controller without reset.
  Missing-path VFS registration must not create a file. These sweeps share the
  source-cache guard; native open/close errors and persistent/partial I/O remain
  outside the model.
  The `restore_io` child adds two public `restore_from` sweeps, with backups
  larger and smaller than their destinations. Each destination write/sync is
  failed in a fresh identical fixture, checking the exact I/O prefix/target and
  healthy rollback writes. Before connection reuse or reopening, the destination
  bytes must match the original. Checks also cover typed graph/index/constraint
  rollback, original extended errors, immediate destination writer and source
  exclusive-lock acquisition, unchanged source bytes and read-only graph,
  integrity, reopening before retry, original-connection reuse, and successful
  retry. Replacement page count/file length, observer visibility, primed caches,
  mutation TEMP state, and subsequent writes surviving reopen are verified.
  Differing page sizes and truncation/deletion faults are outside this restore
  model; source reads have their own coverage below.
  The `backup_io` child adds five tests using the public backup implementation's
  private per-call I/O adapter. Three copy sweeps cover writable, read-only, and
  in-memory sources, with every staging write/sync failed against absent and
  existing destinations. A final-sync/publication error matrix and a real
  competing-publisher hard-link interleaving exercise the filesystem boundaries.
  Tests verify stage ordering and complete contents, original errors, no partial
  publication, preserved destination/source bytes and typed graphs, staging and
  sidecar cleanup, source lock release before reuse, and public retries on the
  same source handle. Retried backups are restored over existing graphs; exact
  identities/indexes/constraints and subsequent writes must survive reopen.
  Staging lifecycle failures have their own coverage below; power loss remains
  outside this backup model.
  The `source_read_io` child adds four sweeps over validation and copy reads:
  writable/read-only backup sources with absent/existing destinations, and
  growing/shrinking restores. Cold caches and a copy-boundary marker ensure
  both phases perform real reads. Each healthy read supplies a byte range and
  occurrence within its phase; matching those targets avoids depending on
  SQLite's validation scan order. A test-only guard serializes these deliberate
  cache resets: bundled SQLite shares their eviction group across connections.
  Tests verify direct `SQLITE_IOERR_READ` errors
  or preserved integrity-check diagnostics, one exact failed read, healthy
  subsequent I/O, unchanged source/protected destination bytes, staging cleanup,
  exact graph/index/constraint preservation, locks released before handle reuse,
  and public retries followed by writes surviving reopen. Destination traces
  prove that some copy read faults follow earlier writes. A fifth test verifies
  path isolation and one-shot consumption without resetting the controller.
  Persistent/partial reads remain outside this model; opening/initialization
  and hot-journal recovery have their own coverage below.
  The `backup_lifecycle` child adds five tests for staging directory/file
  creation, SQLite open/close, and file/directory cleanup. Four matrices cover
  all three source kinds with absent/existing destinations, primary-error
  precedence when cleanup also fails, successful publication despite cleanup
  failure, and healthy controls. They check exact operation order, preserved
  files/typed graphs, source lock release before reuse, recursive cleanup and
  exact leftovers, public retries that leave earlier remnants alone, and
  restorable backups with writes surviving reopen. Removing a retained staging
  hard link must preserve the published backup. Four panic points in a fifth
  test retain native RAII cleanup. Creation/open/close failures are synthetic
  pre-call errors; the copy fault uses the VFS, and no-overwrite errors use
  native hard links. Cleanup faults skip removal while still releasing file
  handles. OS close errors and partial creation/removal remain unqualified.
  The `source_open_io` child adds six tests that register faults before SQLite
  connects. Five sweeps cover existing current-version sources in all three
  open modes and growing/shrinking restores. They fail each healthy main-file
  read target within the connection or initialization phase, preserve the
  original `SQLITE_IOERR_READ`, and stop tracing before validation/copy. A live
  file counter and exclusive-lock probes verify failed-open cleanup. Exact
  bytes and graph/index/constraint snapshots stay intact; restore failures
  precede any destination write. Public retries check access mode, backup/restore,
  observer visibility, file size, and writes surviving reopen. The sixth test
  verifies path/native-VFS isolation and one-shot header failure consumption
  without resetting the controller. These sweeps join the cache-reset guard.
  Native open/close errors and persistent/partial reads are excluded; migration
  reads, empty/new-file initialization, and hot journals have separate coverage.
  The `recovery_io` child adds three tests using a real main/journal pair from a
  terminated writer. The parent proves existing-page spill, file growth, and a
  hot journal before killing/reaping the child, then copies the frozen pair per
  case. Writable sweeps fail each journal read/sync and database replay write/sync
  in healthy traces for both writable open modes, checking exact prefixes and
  extended errors. Before public retry, tests compare the retained journal and
  exact partial main-file image, count live handles, and probe exclusive file
  locks without triggering recovery. Retry must restore original bytes, typed
  bags, indexes, and constraints, remove the journal, and permit backup/restore
  and writes surviving reopen. A refusal matrix covers read-only/restore sources,
  unchanged destination contents, and public retry after explicit writable source
  recovery. An isolation test retries through the same VFS without resetting
  its controller. Recovery tests join the source cache guard and preserve healthy
  I/O after the one injected failure. Deletion/truncation faults, persistent or
  partial failures, damaged journals, and power loss remain outside this model.
  Run `cargo test --locked --lib database::storage_failures` for all eleven slices.
- `tests/read_semantics.rs` holds focused read behavior matrices: nulls and
  types, directions, multiplicity, repeated bindings, comparisons,
  pagination, and parameters.
- `tests/nested_graph_collections.rs` covers recursively typed node, relationship,
  and path containers; mixed selections and forged-record isolation; aliases,
  iteration, aggregation, identity-based grouping/distinctness, writes and
  rollback, mutation snapshots, depth guards, and eager/cursor parity. Depth
  tests exercise explanation and execution on the default thread stack.
- `tests/graph_list_concatenation.rs` covers graph-list joins and appending or
  prepending members, typed rebinding, mixed values, nulls and duplicates,
  all path transports, aggregation and identity, parameters, mutation refresh
  and deletion snapshots, write bags and rollback, scalar addition parity,
  eager/cursor results and partial errors, depth limits, and SQL growth.
- `tests/graph_list_functions.rs` covers `head`, `last`, `tail`, and `reverse`
  on collected entities, all path-list transports, and mixed/nested lists:
  order, duplicates, nulls/empty lists, precise tuple selections and rebinding,
  plain parameter isolation, Unicode/scalar reversal parity, aggregation and
  identity, mutation refresh/deletion snapshots, write bags and rollback,
  cursor errors, depth limits, and bounded SQL growth.
- `tests/graph_conditionals.rs` covers graph-aware `coalesce` and `CASE`:
  typed entity selection/rebinding, identity-based simple selectors, paths
  across every transport, nested/mixed results, nulls/empty collections,
  branch and condition short-circuiting, random value/type consistency,
  parameter isolation, aggregates/grouping/distinctness/unions, mutation
  refresh/deletion snapshots, write bags/rollback, cursor parity/errors,
  depth limits, and bounded SQL growth.
- `tests/entity_list_quantifiers.rs` covers `all`, `any`, `none`, and `single`
  over collected entities and every path-list transport: typed member access,
  null/empty truth tables, duplicates, scoped patterns and nested bindings,
  parameters, aggregates, filters, stored results, mutations and rollback,
  and eager/cursor parity including partial predicate errors.
- `tests/entity_list_comprehensions.rs` covers typed filtering and projection
  over collected entities and every path-list transport, nullable members,
  shadowing and correlated pattern predicates, aliases, grouping/distinctness,
  unions, writes and rollback, mutation refresh/deletion snapshots, and
  eager/cursor parity including partial runtime errors.
- `tests/entity_list_indexing.rs` covers typed indexes and slices over collected
  entities and every supported path producer: ordering/duplicates, graph
  properties and identities, aliases/matches, bounds/nulls, grouping/distinct,
  unions, mutation refresh/deletion snapshots, errors/rollback, and cursor parity.
- `tests/read_cursor.rs` covers incremental and buffered reads, typed bag parity,
  parameter/write preflight, partial errors, cumulative row/byte/VM budgets,
  repeated cursors, cancellation/deadlines, read-only files, snapshot pinning,
  and lock release after early drop, exhaustion, failure, or panic. A one-row
  fetch budget proves that the first row does not require consuming the suffix.
  File tests retain prepared/terminal handles while a second connection writes
  to prove cleanup happens before those handles are dropped.
- `tests/read_cursor_union_all.rs` covers incremental unions of SQLite fragments:
  branch-specific shapes and parameters, duplicates and empty branches, ordered
  and paginated branches, eager parity, whole-query buffered fallback, early
  exit without evaluating later branches, preflight and partial runtime errors,
  cumulative row/byte/VM budgets, cancellation/deadlines, one snapshot across
  branches, drop/error/panic cleanup, prepared-handle reuse, and read-only/busy
  behavior. The scan example and Criterion cases measure full and early-exit
  union consumption as well as single-fragment reads.
- `tests/read_cursor_distinct.rs` covers streaming DISTINCT and distinct UNION:
  typed first representatives, numeric precision, graph and nested identities,
  branch-local distinct/pages, long union chains, duplicate/skip versus output
  accounting, zero-page preflight, unread suffixes and partial errors, shared
  work/control budgets, snapshot pinning, cleanup/reuse, and read-only/busy
  behavior. Cursor deduplication unit tests compare hash-bucket admission with
  the existing equality oracle and force collisions. A cursor unit test uses a
  thread-local, drop-restored clock to expire a deadline after the first row of
  a 100-branch distinct union. Preparation does not race a wall-clock timeout;
  the test checks exact-deadline expiry, terminal iteration, snapshot cleanup,
  refusal to restart with expired options, and subsequent writes. Other cursor
  integration tests retain real-clock deadline coverage. `examples/cursor_distinct.rs`
  validates and measures duplicate-heavy and mostly-unique workloads.
- `tests/read_cursor_hydration.rs` covers incremental SQLite path and graph-list
  hydration: zero hops, cycles, parallel edges, direction and repeated members,
  mixed/nested projections, compact/materialized list equality, branch shaping,
  first-row laziness, per-batch entity reuse and direct-entity seeding, exact
  transfer/result limits, read-ahead errors, cancellation/deadlines, shared work,
  snapshot cleanup/reuse, and read-only/busy behavior. The distinct fingerprint
  unit matrix also checks compact identities against materialized lists.
  `examples/cursor_hydration.rs` validates paths and node lists across batches
  and measures eager, full-cursor, and first-row consumption.
- `tests/query_resources.rs` exercises the options APIs on databases and
  transactions: configurable traversal guards, deadline/cancellation errors,
  SQLite work limits, materialized and final rows/bytes, UTF-8/recursive costs,
  DISTINCT/pagination/UNION boundaries, hydration and traversal input charging,
  per-query reset, and rollback after writes. Database unit tests inject
  cancellation after an actual write; resource unit tests synchronize a
  cancelling thread with running SQLite and verify callback cleanup on panic.
- `tests/uniqueness_constraints.rs` covers the node label/property constraint
  API: installation over existing data, idempotent create/drop, typed recursive
  equality and numeric precision, missing/null values, exact names, independent
  and overlapping declarations, all mutation routes, bags, immediate checks,
  parameter/result failures, transaction abort/rollback, and reopen/connection
  visibility. Schema unit tests cover migrations through version 5, retained
  identities/declarations, and atomic DDL failure restoring both maintenance
  triggers and the version marker.
- `tests/arithmetic_and_distinct.rs` locks down numeric expression typing,
  precedence, null propagation and invalid-operand types, typed ordering, and
  post-decode semantic `DISTINCT`, including pagination and ordering-scope
  boundaries.
- `tests/numeric_errors.rs` covers the finite numeric boundary: zero divisors,
  floating overflow, invalid math domains, aggregate overflow, explicit rejection
  of NaN-producing comparisons, and correct remainder at large finite ratios.
  It exercises parameters, properties, nested collections, predicates, aliases,
  nulls, lazy branches, eager/incremental/buffered reads, cursor reuse, and
  statement/transaction rollback with constraints, index lookups, immediate
  writer lock release, and reopening.
- `tests/numeric_input_errors.rs` covers all fifteen numeric functions' input
  checks: known invalid literals, aliases, list elements, and graph values fail
  during planning; invalid parameters, node/relationship properties, and mixed
  graph selections raise `Error::Type`. It preserves nulls, numeric result kinds,
  duplicate rows, and lazy branches; checks type-only composition and aggregates;
  and exercises eager/incremental/buffered reads, terminal cursor reuse,
  statement/transaction rollback, constraints, immediate writer lock release,
  and reopening. `numeric_errors.rs` retains the separate finite-result checks.
- `tests/aggregate_input_errors.rs` covers strict numeric inputs for `sum` and
  `avg`: static errors, parameters and node/relationship properties, grouped
  bags, mixed graph selections, nulls, empty inputs, numeric promotion, and
  parameter preflight. Composition preserves errors even in unselected
  aggregate results, while lazy branches inside inputs still work. Eager reads,
  both cursor modes, terminal cursor reuse, post-write projections and later
  write stages, transaction rollback, restored constraints, immediate writer
  lock release, and reopening are covered. `numeric_errors.rs` retains the
  separate non-finite aggregate-result checks.
- `tests/string_input_errors.rs` covers all argument positions of the ten string
  functions, including integer positions/lengths: static errors in unused
  branches and empty matches, dynamic parameters and node/relationship
  properties, mixed graph selections, type-only composition and aggregates,
  null precedence, lazy branches, Unicode characters, empty strings, duplicate
  rows, and retained boundary behavior. Eager and both cursor modes, terminal
  cursor reuse, statement/transaction rollback, constraints, immediate writer
  lock release, and reopening are checked through the public API.
- `tests/string_bounds.rs` covers negative positions and lengths for
  `substring`, `left`, and `right`: literals, parameters including `i64::MIN`,
  node/relationship properties, computed and mixed graph selections, empty
  strings, null/type precedence, lazy branches and empty matches, composed and
  aggregate expressions, compiled rebinding after failure, Unicode result bags,
  both cursor modes, rollback and restored constraints, immediate writer lock
  release with an aborted transaction retained, and reopening.
- `tests/integer_conversion.rs` uses explicit integer expectations for binary64
  neighbors around +/- 2^63, exact integer strings and overflow, and malformed
  text. It covers compiled parameter rebinding, stored values, eager/incremental/
  buffered parity, duplicate rows, null-producing writes, wrong kinds, lazy
  branches and rollback when argument evaluation fails. This is a bounded
  independent-value corpus, not a reference-engine differential claim.
- `tests/string_nul.rs` covers NUL at every string position alongside multibyte
  and combining characters. A character-vector oracle checks `size`, both
  substring forms, `left`, and `right` through `i64::MAX` bounds. Tests also
  cover all string predicates, literals/parameters/properties, stored writes and
  reopening, mixed graph selections, nested values/grouping/UNION, duplicate
  rows, eager/compiled and both cursor modes, null/bound errors and rollback,
  and preservation through case conversion, trim, replace, split, and reverse.
  A text-helper unit test checks exact and exceeded SQLite replacement-size
  limits, the original `SQLITE_TOOBIG` code, and successful reuse after failure.
- `tests/list_input_errors.rs` covers strict inputs for `size`, `head`, `last`,
  `tail`, `reverse`, comprehensions, and quantifiers. It checks planning-time
  rejection, parameters/properties and mixed graph selections, nulls, empty
  lists, Unicode strings, duplicate members, lazy runtime branches, type-only
  composition and aggregation, eager/incremental/buffered execution, terminal
  cursor reuse, and statement/transaction rollback with indexes, constraints,
  and reopening. General subscript and UNWIND contracts remain independent.
- `tests/with_distinct.rs` covers typed row deduplication at intermediate
  boundaries: nulls and recursive numeric equality, complete rows and
  wildcards, entity/path/graph-list identity, filtering and ordering scope,
  pagination, aggregation, stable projected random values, hybrid traversal,
  later bag expansion, write multiplicity, and rollback.
- `tests/aggregate_expressions.rs` covers aggregate calls inside arithmetic,
  functions, `CASE`, maps, and scalar list operations; empty/null inputs,
  typed parameters, grouping scope, modifiers, entity identity, runtime paths,
  random values, write multiplicity and rollback, and bounded SQL growth.
- `tests/membership_and_string_predicates.rs` covers typed `IN` membership and
  the three string predicates across parameters, structured values, empty
  collections/strings, Unicode, null, and wrong-kind operands.
- `tests/predicate_truth_tables.rs` compares every supported comparison,
  boolean, membership, string, and null predicate with an independent finite
  Rust oracle across 28 typed operand states, including missing properties and
  mixed numeric precision boundaries.
- `tests/property_invariants.rs` covers exact decoded identifiers and named
  parameters, SQL-looking labels/types/keys, case sensitivity, duplicate
  decoded output names, empty relationship types, recursive node/relationship
  property transport through every mutation form and path hydration, signed
  zero normalization, fail-closed 100-container value-depth boundaries, and
  the independent 128-step expression-syntax guard.
- `tests/property_index.rs` checks that the trigger-maintained
  `node_properties` index equals a recomputation from `nodes` after every
  kind of write and after a rolled-back statement, that indexed lookups keep
  Cypher equality across numeric, boolean, text, list, map, and null values,
  and which query shapes are planned through the index. Updates also check
  SQLite storage kinds, including integer-to-real changes. Database unit tests
  audit index writes to verify unchanged entries are retained, and count SQL
  compilation through an authorizer to verify bulk node/relationship statement
  reuse without timing assertions. Two SQLite planner
  tests cover computed MERGE probe eligibility, normalization/conditional plans,
  and candidate-dependent/volatile/expensive exclusions. Integration coverage
  includes nested/dynamic access, optional and correlated reads,
  guarded errors, duplicate MERGE inputs and matches, whole-pattern matching,
  re-matching after mutations, repeated imports, null keys, and transaction
  rollback, and read-filter errors with empty candidate sets. A coarse
  SQLite-work ceiling distinguishes indexed MERGE matching
  from a label scan without asserting timings or exact instruction counts.
- `tests/entity_values.rs` covers whole node and relationship result values,
  recursive properties, identity, and stored-direction endpoints.
- `tests/write_semantics.rs` holds focused write result/state contracts that do
  not need the longer scenarios in `engine.rs`.
- `tests/matched_mutations.rs` covers matched node/relationship `SET` and
  `REMOVE`, whole-map replace/merge semantics, counters, aliases, parameter
  preflight, no-match behavior, and rollback.
- `tests/property_targets.rs` covers expression targets for property `SET`
  and `REMOVE`: parenthesized bindings, indexes, nested/dynamic selections,
  functions/conditionals, mixed entities, nulls and empty inputs, per-item
  evaluation, duplicate updates and counters, refreshed copies, CREATE/MERGE
  composition and deletion snapshots, invalid types, parameter isolation and
  preflight, uniqueness/property indexes, transaction rollback, and result budgets.
- `tests/delete_mutations.rs` covers node/relationship deletion, detach edge
  cleanup, input-bag target deduplication, pre-delete return snapshots,
  counters, constraint failures, and rollback.
- `tests/delete_expressions.rs` covers node/relationship expression targets:
  indexing, nested and dynamic selections, mixed values, nulls, deduplication
  across columns and rows, evaluation before deletion, refreshed snapshots,
  parameter preflight, forged-record isolation, error and transaction rollback,
  and the separation of internal target values from result budgets.
- `tests/aggregation.rs` and `tests/aggregation_edges.rs` cover implicit
  grouping, typed aggregate behavior, empty inputs, aggregate scope, aliases,
  and structured rejection boundaries.
- `tests/path_aggregation.rs` covers direct path and path-list count/collect,
  distinctness and grouping across every traversal transport; identity,
  direction, parallel edges and segment equivalence; order, duplicates,
  nulls, empty inputs and zero hops; grouped aliases/functions/modifiers,
  later matches, collection composition, unions/pagination, parameter
  isolation and preflight, cursor parity and terminal errors, resource/depth
  guards, SQL growth, and retained numeric/scope/write restrictions.
- `tests/path_predicates.rs` covers direct path and graph-list equality,
  inequality, membership, and null predicates across every traversal transport;
  aliases/selections, chained comparisons, identity/direction/parallel edges,
  bags, nulls and zero hops, grouping/quantifiers/unions, parameter isolation
  and preflight, eager/cursor parity and terminal errors, scalar predicate
  writes and rollback, retained restrictions, resource limits, and SQL growth.
- `tests/path_list_size.rs` covers direct `size` on path-function and legacy
  relationship lists across all traversal transports, aliases, zero hops,
  repeated nodes and segment junctions, bag multiplicity, null/transformed
  lists, filters/aggregation/grouping/modifiers/unions, parameters/preflight,
  retained scope/write restrictions and transaction rollback, eager/cursor
  parity, and plans that avoid entity records and output hydration.
- `tests/collect.rs` covers node/relationship collections and typed distinct
  collection: identity, duplicates, null/empty inputs, order, aliases, grouping,
  typed `UNWIND`, later matches and writes, property/label refreshes, delete
  snapshots, rollback, hybrid traversal, and unions.
- `tests/unwind.rs`, `tests/union.rs`, and `tests/optional_match.rs` cover the
  row-expanding and null/deduplication composition operators, including bag
  multiplicity and clause-local scope.
- `tests/path_unwind.rs` covers expansion of `nodes(path)`,
  `relationships(path)`, and legacy relationship lists into entity bindings:
  aliases, scope, segment junctions, zero hops, cycles, self-loops, parallel
  edges, stored direction, bounded/runtime/shortest producers, later matches,
  collection, distinctness/pagination, unions, hydration, parameters, and
  retained write rejections without side effects.
- `tests/bounded_traversal.rs` covers exact/ranged/zero bounds, outgoing,
  incoming, and undirected traversal, per-hop type/property filters, cycles,
  self-loops, diamonds, parallel edges, dead ends, edge-trail uniqueness,
  composition, the depth-32 SQLite strategy threshold, seeded versus unseeded
  start constraints, and structured rejection boundaries.
- `tests/unbounded_traversal.rs` covers explicit bounds above that threshold,
  open-ended lower-bound forms, Rust trail enumeration, paths and relationship
  lists, mixed SQL/Rust composition, clause-wide relationship uniqueness,
  unions, explain-stage order, and retained rejection boundaries.
- `tests/shortest_paths.rs` covers legacy one/all shortest selection per endpoint
  pair, deterministic ties, diamonds and parallel edges, same-endpoint identity
  versus positive cycles, directions, bounds, type/property/parameter filters,
  paths and relationship lists, path functions, prior bags, `WITH`, `UNION`,
  Rust BFS stage order and structured planning rejections. Traversal unit
  tests cover shortest-specific use of the shared fail-closed work guard.
  Indexed-neighbor tests compare both modes with eager BFS across 64 small
  graph shapes, all directions, zero/positive bounds, and multiple seeds;
  all-path bags also match exhaustive trail enumeration. They verify endpoint
  index use, dense parameter bindings, cache accounting and cancellation.
- `tests/traversal_limits.rs` crosses the public default 100,000-state limit
  while emitting no runtime rows, then proves the connection remains usable.
  Traversal and executor unit tests separately pin exact compact-JSON byte
  accounting, empty relations, shortest-path parity, and query-wide sharing
  across `UNION` branches.
  Constrained shortest-path tests use transfer-row budgets to prove that
  disconnected edges and neighbors beyond the hop bound are not fetched, and
  a failed neighbor load rolls back earlier transaction writes.
- `tests/path_values.rs` covers public named-path results for fixed, bounded,
  mixed, and zero-hop patterns; traversal order and stored relationship
  direction; identity across cycles, diamonds, and parallel edges; direct
  `WITH` aliases; and the current rejection boundary. It also verifies that
  branch-local distinctness/pagination and left-to-right unions shape pending
  path identities before one query-wide batch hydration stage.
- `tests/relationship_list_values.rs` covers fresh bounded legacy
  relationship-list bindings, zero-hop empty lists, traversal order versus
  stored endpoint direction, named-path coexistence, direct and `WITH` aliases,
  semantic distinctness/pagination, unions, query-wide hydration, scope, exact
  re-matching of a bound list in the same clause and after `WITH` across both
  producers, and the explicit unsupported contexts.
- Unit tests beside `planner/lower.rs` should cover binding, scope, semantic
  validation, and logical-plan shape when those details need focused
  diagnosis.
- Unit tests beside `planner/sqlite.rs` cover SQL lowering, internal aliases,
  bind placement, and graph-to-relational joins. They complement rather than
  replace execution tests.
- Unit tests beside `planner/physical.rs` exhaust valid runtime-slot producer
  and consumer permutations with static arguments present in every possible
  gap, and reject duplicate, missing, unreferenced, or reused slots.
- Unit tests in `src/database/tests.rs` cover schema invariants, pending typed-cell
  equivalence, query-wide path-ID collection, direct-entity cache seeding, and
  failure modes requiring direct SQLite access. Its generated-fragment corpus
  asks initialized SQLite to prepare main and traversal-edge statements and
  validates read-only behavior, dense exact bindings, and result shape.
  Integration tests retain semantic and rollback coverage.

For a new query feature, prefer a vertical set of tests: one successful
end-to-end example, relevant zero-row or `null` behavior, dynamic wrong-kind
operands without coercion, multiplicity and direction boundaries,
parameterized values, a nearby semantic or unsupported case, and rollback plus
final graph state when the feature writes.
