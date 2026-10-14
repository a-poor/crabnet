# Third-party materials

Crabnet's own code is licensed under `MIT OR Apache-2.0`; see
[LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE). Third-party
materials retain their own licensing and attribution.

## Vendored openCypher TCK

`tests/tck/vendor/openCypher-2024.3/` contains the unmodified openCypher 2024.3
Technology Compatibility Kit, obtained from the `open-cypher` parser's vendor
tree. Its [Apache 2.0 license](tests/tck/vendor/openCypher-2024.3/LICENSE) and
[NOTICE](tests/tck/vendor/openCypher-2024.3/NOTICE) are retained alongside it,
including the openCypher community attribution and trademark notice. See the
[TCK provenance and instructions](tests/tck/README.md). These files are included
in the Cargo archive; Crabnet's dual-license declaration does not replace them.

## Cargo dependencies

Dependency source archives are fetched separately by Cargo, rather than
vendored into Crabnet's source archive. The locked direct dependencies declare:

| Dependency | Version | Declared license | Use |
| --- | --- | --- | --- |
| open-cypher | 0.2.1 | Apache-2.0 | Cypher parser |
| rusqlite | 0.40.2 | MIT | SQLite bindings and connection management |
| serde | 1.0.229 | MIT OR Apache-2.0 | Serialization support |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | Parameters and stored JSON values |
| tempfile | 3.27.0 | MIT OR Apache-2.0 | Temporary files for backup publication |
| criterion | 0.7.0 | Apache-2.0 OR MIT | Development-only benchmarks |

`libsqlite3-sys` 0.38.2 declares MIT for its Rust wrapper and supplies bundled
SQLite C sources, whose header states that the author disclaims copyright.
The upstream crate retains its own license and source notices. Although
`open-cypher` includes both Apache and MIT license files, its Cargo package
metadata declares **Apache-2.0**; this inventory follows that declaration.

The [locked dependency inventory](docs/dependency-inventory.json) records all
98 third-party packages, their declared expressions, registry checksums, and
hashes of top-level license/notice files available in the cached source
archives. Runtime/build and development-only classifications include
platform-specific dependencies; this is not a list of code linked into every
binary. In particular, Unicode, zlib, and multi-option expressions are retained
verbatim rather than flattened into Crabnet's license choice.

Three cached archives (`anes` 0.1.6, `r-efi` 6.0.0, and `rsqlite-vfs` 0.1.1)
have no top-level license/notice file; their inventory entries deliberately
retain an empty file list. `anes` and `r-efi` describe their license choices in
their READMEs; `rsqlite-vfs` declares MIT in its Cargo manifest. None of these
archives is copied into Crabnet's source package. This distinction is retained
for any later binary-distribution review.

Run `python3 scripts/dependency-inventory.py --check` to verify the inventory
against cached Cargo sources, or omit `--check` to regenerate it for review.
This helper requires Python 3.11 or newer.

The inventory describes the reviewed lockfile, not all versions allowed by
Cargo.toml. Regenerate and review it when dependencies change. Applications
that redistribute compiled dependencies need their own distribution inventory;
this source-package review is not a certification of a downstream binary.
