# API and storage compatibility

This policy starts with the current `0.1.0` development line. Crabnet remains
experimental pending the [milestone 11 qualification gates](roadmap.md#11-production-qualification--in-progress).
The retained baselines below are historical main commits, not published release
pairs. They establish regression coverage for existing formats; they do not
retroactively qualify those commits as supported production releases.

## Versioning and public API

The Cargo package version and SQLite schema version are separate. `0.1.0`
development commits have written five different schemas. A package version
alone cannot identify one of these historical files: record the exact writer
revision and inspect its schema with the appropriate Crabnet version.

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

## Supported storage upgrades

Schema version **5** is current. Crabnet identifies its files using SQLite's
`application_id` (`0x43524142`) and `user_version`; editing these markers does
not convert a database.

| Input | Writable open | Read-only open / restore source |
| --- | --- | --- |
| Empty, unmarked SQLite file | Initialize only with `OpenOrCreate` | Reject |
| Main schema 1: nullable single label | Atomically upgrade to 5 | Reject; writable upgrade required |
| Main schema 2: multiple labels and label index | Atomically upgrade to 5 | Reject; writable upgrade required |
| Main schema 3: property index | Atomically upgrade to 5 | Reject; writable upgrade required |
| Main schema 4: node uniqueness constraints | Atomically upgrade to 5 | Reject; writable upgrade required |
| Schema 5: selective index/constraint maintenance | Open | Open / validate and restore |
| Experimental single-label schema 2, unknown/future version, foreign or unrelated nonempty database | Reject | Reject |

Read/write mode requires an existing file. Read-only opening and `restore_from`
never migrate their input. Opening validates the expected layout, not complete
file integrity; restore additionally performs its documented integrity checks.
Malformed, externally modified, or corrupted files have no automatic repair
guarantee. The [opening and recovery contracts](semantics.md#file-opening-and-contention)
still apply, including writable hot-journal recovery.

Supported upgrades preserve node and relationship IDs, label insertion order,
property value kinds and precision, relationship multiplicity/endpoints, and installed
uniqueness declarations. Derived indexes must continue to select and enforce
the same data. Migration DDL, backfills, and the version marker commit together;
ordinary errors leave the previous persisted schema and data intact. Process
death coverage is narrower than power-loss or I/O-fault qualification.

Early writers stored explicit top-level null entries. Migration retains those
entries, including their appearance in whole-entity property maps. Property
access still returns null; `keys()` and `properties()` omit them and uniqueness
constraints do not reserve their values. Current writes normalize new null
properties to absence. Nested nulls remain ordinary list/map members.

Patch releases in this minor line keep schema 5 and support valid main schemas
1–4 as upgrade inputs. A new incompatible on-disk format requires a new schema
version. Format changes or removal of an upgrade path require a minor release
and explicit upgrade notes.
Retain old fixtures even if a future release deliberately rejects a format, so
that refusal and any documented intermediate-upgrade path remain tested. Do
not silently reinterpret existing schema markers. Compatibility with older
readers is not promised merely because a file still uses the same marker.

## Upgrade and application rollback

There is no automatic downgrade. Keep the old executable/dependency resolution,
configuration, and a verified **pre-upgrade backup** until accepting the upgrade.

1. Stop application writers and close other graph handles. With the old version,
   recover any interrupted transaction and create a backup at a fresh path.
   Verify that the old version can reopen it and that application checks pass.
   Writers predating `backup_to` require a clean shutdown and a closed-file copy
   under the supported DELETE-journal configuration. Never copy only the main
   file from a live or unrecovered database, and never discard its hot journal.
2. Preserve that backup unchanged. Copy it to a new working path and use the
   new version's writable open to migrate the copy. Verify application queries,
   identities, counts/bags, constraints, and the ability to reopen it. Produce
   and restore a new backup as part of the application's acceptance checks.
3. Switch the quiescent application to the verified upgraded copy. Keep all
   processes on the same Crabnet version; concurrent old and new writers are
   outside the supported contract.
4. If acceptance fails, close all new handles and restart the old application
   against a fresh copy of the original pre-upgrade backup. Do not open the
   upgraded file with an older engine or lower `user_version`. Writes accepted
   after upgrading are absent from the old backup and need an application
   replay/reconciliation procedure before rollback can preserve them.

To restore an old-schema backup with the current API, copy it, open the copy
writably to upgrade, call `backup_to` on that upgraded database, and pass the
new snapshot to `restore_from`. Preserve the original backup for rollback.
Restore replaces the destination graph and constraints and requires quiescent
application connections; it is not a merge.

If an upgrade cannot proceed, keep the original file and journal, inspect the
structured error, and address access/locking or the documented format mismatch
before retrying. For unsupported formats, use a known compatible writer to
export/rebuild or a documented intermediate upgrade. Applications such as Git
Graph can reimport authoritative Git data, but must separately preserve local
annotations and remap references through domain keys. Rebuilding does not
promise identical opaque graph IDs. No raw-SQL repair recipe is implied.

## Retained evidence and release gate

[Retained fixtures](../tests/fixtures/compatibility/README.md) cover each main
schema 1–5. They were produced through the public API by five pinned historical
writers using their own locked dependencies. Versions 1–3 are clean closed-file
copies; versions 4–5 use the historical `backup_to` API. The original writers
reopened their own snapshots before retention. A manifest records provenance
and SHA-256 hashes; JSON oracles record the old persisted graph independently
of current migration code.

`tests/storage_compatibility.rs` runs in the ordinary test matrix without
fetching history. It checks upgrades, typed data/identity and relationship bags,
index/constraint maintenance, failed-migration rollback and retry, read-only and
unsupported-format refusal, and restoration through a migrated copy while
preserving the original rollback source. It complements generated schema unit
tests and the opening, backup, and process-recovery suites.

Before qualifying a named release, retain fixtures from every supported released
writer, with its tag and exact commit, lockfile, generator, and hashes. Run the
candidate against those inputs on the full support matrix, perform the
application upgrade/rollback rehearsal, and record the exact candidate, source
versions, results, and limitations. Append fixtures when a release changes the
writer even if its schema number stays the same; never replace historical
evidence to make a regression pass. The current commit baselines are not
release-to-release evidence. That evidence and the rest of milestone 11 remain
release gates.
