# Supported configuration and qualification

This is the initial configuration targeted by milestone 11, defined on
2026-10-09. Crabnet remains experimental. The first hosted matrix and lifecycle
runs passed at `683b8e9`. The [compatibility policy](compatibility.md) and retained
historical format fixtures are in place; required-check enforcement, named
release-pair validation, fault testing, and the release record remain open gates in
[roadmap](roadmap.md#11-production-qualification--in-progress).

## Initial matrix

| Operating system / architecture | Rust target | CI runner | Toolchains |
| --- | --- | --- | --- |
| Ubuntu 24.04 LTS, x86-64, glibc | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `1.99.0` and current `stable` |
| macOS 26, Apple Silicon | `aarch64-apple-darwin` | `macos-26` | `1.99.0` and current `stable` |

Rust 1.99 is the deliberately conservative minimum declared in `Cargo.toml`,
matching the measured engine and Git Graph baseline. Older toolchains have not
been qualified; this is not a claim that they cannot compile the source.
`1.99.0` is the reproducible compiler baseline. Rolling `stable` is a required
compatibility check, with its exact version recorded in each run; it does not
silently replace the baseline. Rustup, Cargo, a C compiler, and the platform SDK
or development headers are build prerequisites. Lifecycle measurements also
require Python 3.11 or newer; CI explicitly installs Python 3.14 and records its
exact patch version rather than relying on the runner's system Python.

Runner labels are selected from GitHub's
[hosted runner matrix](https://docs.github.com/en/actions/reference/runners/github-hosted-runners).
Images receive updates; retain the runner image version, OS, compiler, and
revision from the workflow logs when qualifying a release. No Windows, Intel
macOS, Linux ARM, musl, mobile, WebAssembly, or other OS version is qualified
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
  schema 1–4 upgrades to version 5, and rollback using a pre-upgrade backup.
  Retained historical commit fixtures run in the regular matrix; named
  release-to-release qualification remains a milestone 11 gate. Crabnet remains
  experimental. [Allocation-failure tests](semantics.md#storage-allocation-failures)
  cover `SQLITE_FULL` during writes using a page-count cap.
  [Commit I/O tests](semantics.md#commit-io-failures) cover modeled one-shot
  journal/database write and sync errors with healthy rollback I/O.
  [Migration I/O tests](semantics.md#migration-io-failures) apply that model to
  retained schema 1–4 upgrades, verifying the old format before public retry.
  [Restore I/O tests](semantics.md#restore-io-failures) apply it to destination
  writes/syncs for larger and smaller backups, checking exact rollback, unchanged
  sources, lock release, and successful retry on the original connection.
  These and process-death recovery do not establish filesystem ENOSPC, persistent/partial
  I/O failure, or power-loss guarantees.

Dependency, feature, compiler-baseline, and configuration changes require a full
matrix run and refreshed workload evidence. Library consumers resolve their own
lockfiles; a different resolution is outside this exact qualification record
until validated. Keep the baseline toolchain in the workflow and `rust-version`
in `Cargo.toml` consistent when deliberately raising the minimum.

## Automated checks

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, version
tags (`v*`), Monday at 08:23 UTC, and manual dispatch. Every OS/toolchain pair runs
the same [check script](../scripts/check.sh):

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
log capture; matrix entries do not cancel each other. Check artifacts retain
the tested revision, exact compiler, runner image, SQLite dependency features,
and command output for 30 days. Preserve release evidence outside expiring
artifacts. No automated publish, deployment, or release creation occurs.

All four `Checks (...)` matrix jobs must pass before merging. Enforcement is
currently manual: on 2026-10-09, both the repository rulesets and classic branch
protection APIs returned HTTP 403 for this private repository, stating that
GitHub Pro or public visibility is required. Neither account nor repository
settings were changed. When the repository's plan supports protection, require
those four check names and verify enforcement. The workflow's successful runs
do not close this remaining enforcement gate.

## Workload and release gates

Weekly, `v*` tag, and manual runs with `workloads: true` additionally run the
release-profile dependency-graph lifecycle on both platforms, after the full
check matrix passes. Each job uses fresh databases and three serial trials at
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
- The versioned [hosted evidence record](measurements/hosted-ci-2026-10-09.json)
  preserves run/job identities and results, environment logs, artifact file
  SHA-256 hashes, and all 12 raw lifecycle reports. Artifact uploads succeeded
  and the downloaded reports were reviewed. Shared-runner timings remain
  observations, not a new performance guarantee. Required-check enforcement
  remains unavailable under the current private-repository plan, so the
  automated-release-check roadmap item remains open for that specific gap.
