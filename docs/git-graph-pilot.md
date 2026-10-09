# Git Graph downstream pilot

Git Graph completed its sustained application milestone on 2026-10-09 using
Crabnet's public Rust API. All declared correctness, performance, memory,
concurrency, process-interruption, and backup/restore gates passed. This is
evidence for the [application pilot checklist](roadmap.md#pilot-completion-checklist);
production release qualification remains a separate milestone.

The application lives in the sibling checkout `../../../exp/git-graph` relative
to this document. Its versioned [full report](../../../exp/git-graph/docs/pilot.md#milestone-2-acceptance-results-2026-10-09)
and [raw acceptance JSON](../../../exp/git-graph/docs/milestone-2-acceptance.json)
contain the protocol, every timing sample, cycle results, fault checkpoints,
commands, fixture hashes, and source/binary fingerprints. These links assume the
documented `code/libs/crabnet` and `code/exp/git-graph` development layout.

## Baseline and contract

- Crabnet: clean `7601cdd79aa0f3a46263ac007aae22f217c6ffa4` throughout measurement.
  No engine code changed. This report and roadmap link were added afterward.
- Git Graph: uncommitted milestone 2 implementation on
  `36cd00444ecea443d275596a22895b79138e2fa0`; implementation SHA-256
  `5ddd4730dc6791d1b3cf70f2924f0d2ba0e3e591ae5eeabb29368d3f6b722cc0`.
  The measured source was subsequently committed as
  `52e62f9f3b2b61187fff5a46d3a7235cb66d6137`. Recomputing the harness fingerprint
  from that commit's 24 source/build files matches the recorded hash exactly.
  The [post-acceptance provenance](../../../exp/git-graph/docs/pilot.md#post-acceptance-provenance)
  preserves the original run's base revision, dirty status, and measurements;
  the later commit identifies the source and does not imply an acceptance rerun.
- Host: Apple M2, eight physical cores, 16 GiB RAM, macOS 26.5.1 arm64;
  Rust/Cargo 1.99.0, Git 2.50.1, Python 3.14.7.
- Local DELETE-journal database, one repository per database, bounded in-memory
  extraction. Unique immutable commits, ordered parent edges, authoritative
  branch refresh, retained unreachable history, atomic generation publication.
- Parameterized eager lookup/first-parent reads; public transactions,
  constraints, resource controls, typed values, reopen, `backup_to`, and
  `restore_from`. The contention probe also uses a public incremental cursor.

The Git Graph design handoff records the engine baseline's 901 tests, 143
benchmark smoke cases, 11 doctests, formatting, and Clippy. Those engine checks
were not rerun for this application-only change. Git Graph's final validation
passed 30 Rust tests, two Python fixture tests, doctests (none defined), formatting,
Clippy with warnings denied, release builds, and whitespace checks.

Review on 2026-10-09 verified the committed source fingerprint and raw report's
`passed` result, `acceptance_complete: true`, all 14 gate results, and 100 completed
cycles. The report also corrects milestone 1's prose database size to 274,432 bytes,
matching its unchanged raw result. Crabnet milestone 10 is closed for this pilot's
recorded scope; no engine changes or acceptance rerun were needed for closeout.

## Acceptance evidence

Deterministic `mixed-v1`, seed 20261009: 1,000 and 10,000 commits, chains and
merge diamonds, four branches, fixed metadata, three serial initial/reimport
trials per size. The targets were declared before measurements and not relaxed.

| 10,000-commit gate | Observed | Target |
| --- | ---: | ---: |
| Initial import, slowest of three | 14.338 s | <=60 s |
| Unchanged reimport, slowest of three | 10.394 s | <=15 s |
| Peak importer RSS, maximum | 24.469 MiB | <=256 MiB |
| Warm lookup p95, 100 samples | 0.486 ms | <=50 ms |
| Warm 100-row log p95, 100 samples | 24.018 ms | <=500 ms |

All 100 update/import/query/reopen cycles passed in 218.138 seconds. The final
generation retained 1,100 commits and 1,169 parent edges, including 590 commits
no longer reachable in the final source snapshot. Twenty injected cancellation
or Rust-side errors preserved the entire prior graph. Audits compare complete
properties, positioned edge bags, branch state, constraints, and entity IDs.

A retained incremental cursor forced a busy commit failure and exact rollback.
After releasing it, the first whole-operation retry succeeded. An independent
reader observed 13 complete old and seven complete new generations, with zero
busy failures and 36.765 ms observation p95. The probe used a 100 ms busy timeout,
25 ms backoff, and at most three retries; the ordinary CLI reports busy directly.

Large fault probes staged 2,000 new nodes. SIGKILL after spilled writes left a
live rollback journal and recovered exactly the old graph on writable reopen.
SIGKILL after commit preserved the entire new graph and its recorded identities.
A real 2,883,584-byte public-API backup restored the exact original graph after
connections were closed. The real Crabnet repository's 70 commits, 69 parents,
and single branch matched an independent Git oracle and remained unchanged.

## Findings, reproduction, and limits

The initial application `OPTIONAL MATCH` lookup scanned unrelated parent edges
and failed the read-latency targets. Two indexed one-hop reads fixed it without
engine changes or higher budgets. A regression failed before the change and
passes afterward under the same 5,000 SQLite-step per-query ceiling.

A hot rollback journal cannot be recovered by opening read-only. The application
now documents an explicit writable recovery command that needs only the database;
this follows Crabnet's existing recovery contract. Both original preflight
findings are preserved in the application's report.

From the Git Graph checkout, with Crabnet clean at the recorded revision:

```sh
cargo build --release --locked --bin git-graph --example pilot
python3 scripts/pilot.py --output /tmp/git-graph-m2-acceptance \
  --real-repo ../../libs/crabnet
```

Use a new output directory. The harness creates all fixtures/databases there and
only reads the real repository. The full report supplies verification commands
and an optional hardware descriptor. The checked-in JSON preserves raw timing
evidence; disposable database and command-output files remain local artifacts.

The results qualify this bounded application workload on the recorded host.
They do not establish power-loss durability, disk/I/O fault handling, WAL or
network filesystem support, larger graph capacity, or a production platform
matrix. Reads remain eager and restore requires quiescent connections. Further
ancestry queries and Crabnet milestone 11 qualification remain separate work.
