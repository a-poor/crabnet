# Supported configuration and qualification

This records the configuration targeted by milestone 11 and the current CI
coverage. Crabnet remains experimental. The initial configuration was defined on
2026-10-09; the first hosted matrix and lifecycle runs passed at `683b8e9`.
Routine CI was narrowed on 2026-10-13 to conserve Actions usage.
The [compatibility policy](compatibility.md) and retained historical format
fixtures are in place; required-check enforcement, named
release-pair validation, fault testing, and the release record remain open gates in
[roadmap](roadmap.md#11-production-qualification--in-progress).

## Current CI baseline

| Operating system / architecture | Rust target | CI runner | Toolchains |
| --- | --- | --- | --- |
| Ubuntu 24.04 LTS, x86-64, glibc | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `1.99.0` |

Rust 1.99 is the deliberately conservative minimum declared in `Cargo.toml`,
matching the measured engine and Git Graph baseline. Older toolchains have not
been qualified; this is not a claim that they cannot compile the source.
`1.99.0` is the reproducible compiler baseline and matches the local development
compiler. A separate rolling `stable` job is paused; the first hosted stable
jobs resolved to the same 1.99.0 compiler. Rustup, Cargo, a C compiler, and the
platform SDK or development headers are build prerequisites. Lifecycle measurements
also require Python 3.11 or newer; CI explicitly installs Python 3.14 and records its
exact patch version rather than relying on the runner's system Python.

Runner labels are selected from GitHub's
[hosted runner matrix](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Images receive updates; retain the runner image version, OS, compiler, and
revision from the workflow logs when qualifying a release.

The original matrix also checked macOS 26 Apple Silicon (`aarch64-apple-darwin`,
`macos-26`) on both Rust 1.99.0 and rolling stable. That hosted coverage is paused
to conserve Actions usage. The historical evidence below remains valid for its
recorded revisions; local macOS checks can still use the complete check script.
Run fresh platform-specific checks, lifecycle measurements, and file-locking,
recovery, and backup tests before qualifying a release for macOS.

No Windows, Intel macOS, Linux ARM, musl, mobile, WebAssembly, or other OS version is qualified
by these entries. Extending the matrix requires the same checks and a review
of file locking, process recovery, and backup behavior on the new configuration.

## SQLite and operating contract

- Use the checked-in `Cargo.lock` with `--locked`: currently `rusqlite 0.40.2`,
  `libsqlite3-sys 0.38.2`, and its bundled SQLite **3.53.2**. The enabled rusqlite
  features are `bundled`, `functions`, `hooks`, and `backup`. Use the bundled
  crate's default native build settings, including `SQLITE_THREADSAFE=1`;
  custom SQLite libraries, feature unification that changes that build, and
  compile-time overrides need separate qualification. The system `sqlite3`
  executable is not the engine used by Crabnet.
- Use in-memory databases or local files with ordinary reliable file locking.
  The file-backed contract is DELETE rollback journaling with SQLite's default
  FULL synchronous setting. Crabnet enables foreign keys on every connection;
  it does not normalize arbitrary externally changed SQLite settings. Network
  filesystems, WAL, external raw-SQL writers, and custom VFS/configuration are
  outside this initial contract. Record the actual filesystem in release evidence.
- Use independent connections serially, with at most one active writer. Long
  reads and incremental cursors can block commit. Handle `Error::Busy` with
  application policy; retry the entire rolled-back operation where appropriate.
  Read-only opening cannot recover a hot journal: recovery needs writable access.
  Restore requires quiescent application connections. See the
  [opening](semantics.md#file-opening-and-contention) and
  [backup/restore](semantics.md#backups-and-process-recovery) contracts. Backup
  publication also requires same-filesystem hard links.
- The query contract is the explicitly documented subset in
  [coverage.md](coverage.md) and [semantics.md](semantics.md), with the reviewed
  openCypher 2024.3 allowlist and exclusions. It is not full Cypher conformance.
  Known wrong-answer cases in the chosen release surface still require resolution
  or an explicit rejection before qualification. Resource counters and deadlines
  have documented boundaries; they are not a whole-process memory or time cap.
- The [compatibility policy](compatibility.md) defines pre-1.0 API versioning,
  the current schema-5 format, and future release upgrades. There is no
  first-release compatibility obligation for unpublished development formats.
  Crabnet remains experimental. [Allocation-failure tests](semantics.md#storage-allocation-failures)
  cover `SQLITE_FULL` during writes using a page-count cap.
  [Commit I/O tests](semantics.md#commit-io-failures) cover modeled one-shot
  journal/database write and sync errors with healthy rollback I/O. Separate
  journal-deletion tests distinguish recovered rollback before unlink from
  committed writes after a reported post-deletion error. Commit errors do not
  guarantee rollback: reconcile the persisted outcome before retrying.
  [Initialization I/O tests](semantics.md#empty-and-new-file-initialization-io-failures)
  cover missing paths, zero-byte files, and empty SQLite databases. They verify
  uninitialized contents after failure, exact rollback, released handles/locks,
  and successful public retry without removing the leftover file. A failed
  create can leave a zero-byte file. Path isolation and one-shot consumption
  are checked with a final-sync fault and the same controller on retry.
  [Restore I/O tests](semantics.md#restore-io-failures) apply it to destination
  writes/syncs for larger and smaller backups, checking exact rollback, unchanged
  sources, lock release, and successful retry on the original connection.
  [Backup I/O tests](semantics.md#backup-copy-and-publication-failures) cover
  staging write/sync and final file-sync/publication errors, plus a real hard-link
  publication race, with preserved sources/destinations, staging cleanup, and
  successful public retries from writable, read-only, and in-memory sources.
  [Source-read tests](semantics.md#backup-and-restore-source-read-failures) cover
  one-shot read failures during backup/restore validation and copying, including
  SQLite integrity-check diagnostics, preserved files/graphs, released locks,
  staging cleanup, and public retries on the original handles.
  [Staging lifecycle tests](semantics.md#backup-staging-lifecycle-failures)
  cover modeled creation/open/close and cleanup failures, primary-error
  precedence, successful publication despite cleanup failure, retained staging
  artifacts, public retries, and cleanup during panic unwinding.
  [Source-opening read tests](semantics.md#source-opening-and-initialization-read-failures)
  cover connection header and initialization reads for existing current-version
  graphs in all access modes and during restore source opening. They check
  original errors, file/lock cleanup, unchanged source and destination contents,
  no destination writes after failed source opening, and public retries.
  [Hot-journal recovery tests](semantics.md#hot-journal-recovery-io-failures)
  inject one-shot journal read/sync and database replay write/sync errors after
  terminating a writer. They check retained recovery information, partial replay,
  handle/lock release, successful writable retry, and read-only/restore refusal
  until explicit writable source recovery.
  These and process-death recovery do not establish filesystem ENOSPC, persistent/partial
  I/O failure, or power-loss guarantees.

Dependency, feature, compiler-baseline, and configuration changes require the
full checks and refreshed workload evidence on every platform claimed by the
release. Routine CI currently covers only the baseline above. Library consumers
resolve their own lockfiles; a different resolution is outside this exact qualification record
until validated. Keep the baseline toolchain in the workflow and `rust-version`
in `Cargo.toml` consistent when deliberately raising the minimum.

## Automated checks

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, version
tags (`v*`), and manual dispatch. Weekly scheduling is paused. One Ubuntu 24.04
job on Rust 1.99.0 runs the complete [check script](../scripts/check.sh):

```sh
bash scripts/check.sh
```

Use `RUSTUP_TOOLCHAIN=1.99.0 bash scripts/check.sh` after installing that toolchain
and its `rustfmt`/`clippy` components to reproduce the compiler baseline locally.

The script runs `cargo test --locked --all-targets` (unit/integration tests,
vendored TCK, examples, and Criterion smoke cases at both 1,000 and 10,000 nodes),
doctests, formatting, Clippy with warnings denied, and whitespace checks. It
refuses `CRABNET_TCK_UPDATE` so checks cannot accidentally regenerate expectations.
CI also verifies that dependency and TCK pins were not changed by the run.
No TCK download, pin regeneration, or reduced benchmark-size override is needed.

GitHub actions are pinned to immutable commits, checkout does not persist
credentials, and workflow permissions are read-only. Failures propagate through
log capture. New pushes cancel obsolete runs for the same branch or pull
request; manual and tag runs are not automatically canceled. Check artifacts
retain the tested revision, exact compiler, runner image, SQLite dependency features,
and command output for 30 days. Preserve release evidence outside expiring
artifacts. No automated publish, deployment, or release creation occurs.

`Checks (ubuntu-24.04, Rust 1.99.0)` must pass before merging. Enforcement is
currently manual: on 2026-10-09, both the repository rulesets and classic branch
protection APIs returned HTTP 403 for this private repository, stating that
GitHub Pro or public visibility is required. Neither account nor repository
settings were changed. When the repository's plan supports protection, require
that check name and verify enforcement. The workflow's successful runs
do not close this remaining enforcement gate.

## Workload and release gates

Only manual runs with `workloads: true` additionally run the release-profile
dependency-graph lifecycle on Ubuntu 24.04 with Rust 1.99.0, after checks pass.
Tag pushes still run the full checks; they do not automatically run measurements.
The lifecycle job uses fresh databases and three serial trials at
1,000 and 10,000 packages, with 20 warm query samples and batch size 128.
The existing workload independently verifies graph bags, identities, rollback,
deletion, and reopening. JSON records timings, RSS, and binary hashes; environment
and build logs accompany it in 90-day artifacts. Reproduce with the commands in
[dependency-graph.md](dependency-graph.md#reproduce-the-lifecycle-workload).

These jobs fail on correctness errors. Shared-runner timings are measurements
for review, not enforced hardware-independent latency guarantees. Before a
release, run the workflow on the exact candidate with workloads enabled, preserve
the run URL and reports, and investigate regressions against comparable hardware.
Tag checks provide additional evidence; a green tag does not publish a release.

The downstream [Git Graph acceptance workload](git-graph-pilot.md) is an explicit
additional release gate: rerun its declared 100-cycle, performance, contention,
process-recovery, and backup/restore checks against the candidate Crabnet revision
with the measured application source pinned. Retain the exact Git Graph revision,
source fingerprint, Crabnet revision, host, commands, raw JSON, and all gate
results. This workflow does not clone or modify the separate application repo,
and the older pilot result is not evidence for a newer candidate. Full Criterion
timing runs remain a release gate when performance-sensitive code or dependencies
change; use comparable hosts and retain their reports. The remaining compatibility,
correctness, and fault-model gates still apply before any production claim.

## Qualification evidence

- On 2026-10-13, the [latest push run](https://github.com/a-poor/crabnet/actions/runs/38410873346)
  at `53eb024` and the preceding three failed runs were blocked before any test
  steps started. GitHub reported an account payment or spending-limit issue;
  these are not test failures. The annotation does not identify whether the
  included allowance, a budget, or a payment problem caused the block. Reducing
  this workflow's usage does not itself clear that account-level restriction.
- Existing application evidence: the [Git Graph pilot](git-graph-pilot.md),
  measured on macOS 26.5.1 arm64 with Rust 1.99.0 and Crabnet `7601cdd`.
- Local validation on 2026-10-09, macOS 26.5.1 arm64 and Rust 1.99.0, for the
  working tree adding this workflow over `7601cdd`: `bash scripts/check.sh`
  passed 901 unit/integration/example tests, 143 benchmark smoke cases, 11
  doctests, formatting, Clippy, and whitespace checks. Actionlint 1.7.12 and
  shell syntax checks passed; the TCK rewrite guard rejected both a set and an
  empty update variable. The exact lifecycle measurement step passed all six
  trials with Python 3.14.7, preserving identity, rollback, and reopen checks.
  An initial system-Python 3.9 attempt failed in report hashing; explicit Python
  setup fixes that prerequisite. These local checks do not validate a hosted
  runner or GitHub's trigger, artifact, and required-check configuration.
- The first [push run](https://github.com/a-poor/crabnet/actions/runs/37999916541)
  passed all four matrix jobs at
  `683b8e9022684e64c2b12acf1cb90432c7ecf04d`. Each passed 901 tests, 143 benchmark
  smoke cases, 11 doctests, formatting, Clippy, and dependency/TCK pin verification.
  Lifecycle jobs were skipped as specified for an ordinary branch push.
- The [manual run with workloads enabled](https://github.com/a-poor/crabnet/actions/runs/38009609675)
  passed the same four checks and both lifecycle jobs at that exact revision.
  All 12 lifecycle trials passed: three each at 1,000 and 10,000 packages on
  each platform, with repeat identity, rollback, and reopen identity verified.
  Rust `stable` resolved to 1.99.0 on both platforms; lifecycle jobs used Python
  3.14.8. Runner images were `ubuntu24 20261004.327.1` and
  `macos26 20260907.0351.1`.
- Artifact uploads succeeded and the downloaded reports were reviewed.
  Shared-runner timings remain observations, not a new performance guarantee. Required-check enforcement
  remains unavailable under the current private-repository plan, so the
  automated-release-check roadmap item remains open for that specific gap.
