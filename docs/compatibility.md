# API and storage compatibility

Compatibility commitments begin with the first published `0.1.0` release.
There are no earlier released databases or APIs to support. Files written by
unreleased development commits are not a first-release compatibility obligation;
rebuild those databases from their source data if needed.

## Versioning and public API

The Cargo package version and SQLite schema version are separate. The first
release establishes the baseline for future compatibility guarantees.

For releases made under this policy:

- Within a `0.x` minor line, patch releases preserve public Rust source
  compatibility and the documented query, value, error-category, and transaction
  contracts. Breaking changes require the next minor version, release notes,
  and an application migration procedure. Prefer deprecating an API before
  removing it in a later minor line. A future `1.0` release must explicitly
  establish its longer-term support window.
- Additive APIs and extensions permitted by `#[non_exhaustive]` are allowed.
  Consumers must handle unknown enum variants where required by the Rust API.
  Exhaustive public structs and enums remain subject to source compatibility;
  adding a required field is not a patch-compatible extension.
- The supported query subset is defined by [semantics](semantics.md) and
  [coverage](coverage.md). Bug fixes bring behavior into that contract and may
  change erroneous results; release notes must identify observable corrections.
  New syntax must preserve existing supported queries. Broad openCypher
  compatibility is not implied.
- Exact error messages, generated SQL, query plans, explanation prose, internal
  tables/triggers, and performance observations are not stable interfaces.
  Public error categories and documented result types are. Element IDs are
  opaque strings: their format is not an API, but their values survive reopen,
  supported upgrades, and backup/restore of the same graph. Reimporting a graph
  may assign new IDs; applications should retain their own domain keys.
- Rust 1.99 is the current minimum. Raising it requires a minor release before
  `1.0`, updated support/release notes, and full matrix validation. Dependency
  or SQLite changes must preserve these contracts and receive the checks in
  [support.md](support.md).

## Current storage format

Schema version **5** is current. Files use SQLite's `application_id`
(`0x43524142`) and `user_version`; editing these markers does not convert a
database. Writable opening can initialize an empty file with `OpenOrCreate`.
Read-only opening and restore sources require the current format. Foreign,
unknown/future, and unsupported layouts are rejected.

The engine still contains legacy schema 1–4 migration code and synthetic unit
tests. Those are implementation details for pre-release development files,
not a promise of support or historical-file qualification. No archived writer
fixtures or pre-release migration fault sweeps are retained.

Current-format tests generate temporary databases and verify reopening, typed
values, identities, constraints, backup/restore, refusal of invalid files, and
recovery. Opening checks the expected layout; restore additionally performs its
documented integrity checks. Malformed or externally modified files have no
automatic repair guarantee. See [opening and recovery](semantics.md#file-opening-and-contention).

Patch releases in the 0.1 line preserve schema 5 and the documented storage
contract. A new incompatible format requires a new schema version and a minor
release with an explicit upgrade procedure. Compatibility with older readers
must not be inferred solely from matching schema markers.

## Upgrade and application rollback

For a future release that changes the format, define and test the upgrade path
from supported **published** versions. Add migration fixtures when that upgrade
is needed, rather than retaining every development revision.

Keep the previous application and a verified pre-upgrade backup until accepting
an upgrade. Work on a copy, verify application results and identities, and test
backup/restore before switching the application. Roll back by reopening a copy
of the untouched backup with the old application; never lower `user_version` or
assume an older reader can open the upgraded database.

Rollback loses writes made after the backup unless the application can replay
them. Rebuilding from authoritative data can assign new graph IDs, so retain
domain keys and reconcile application references. These are future upgrade
planning rules, not additional gates for the first release.
