#!/usr/bin/env python3
"""Author historical fixtures into a NEW directory, or verify retained hashes.

Requires Python 3.12+, Git history, Rust 1.99, and the locked historical crates.
Normal tests never run this generator or fetch/build historical dependencies.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
WRITERS = {
    1: "f2a5b662a46d1d13af724c168bbefe8debfb06f3",
    2: "b3c71e825b0bf09a6976b1f702e82091de7628c5",
    3: "b409846d3380088998b908d78cb17afb9e89d563",
    4: "9453f5b1e7b102f0e53fe6da1b6de2b0bd7b9eed",
    5: "683b8e9022684e64c2b12acf1cb90432c7ecf04d",
}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--output", type=Path)
    mode.add_argument("--verify", action="store_true")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if args.verify:
        manifest = json.loads((HERE / "manifest.json").read_text())
        for entry in manifest["fixtures"]:
            for name, digest in entry["files_sha256"].items():
                if sha256(HERE / name) != digest:
                    raise SystemExit(f"fixture hash mismatch: {name}")
        print("All retained fixture and oracle hashes match.")
        return

    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    template = (HERE / "producer.rs").read_text()
    manifest = {
        "format_version": 1,
        "kind": "pinned-main-commit baselines; not published release fixtures",
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True),
        "cargo": subprocess.check_output(["cargo", "-V"], text=True).strip(),
        "producer_sha256": sha256(HERE / "producer.rs"),
        "fixtures": [],
    }
    with tempfile.TemporaryDirectory(prefix="crabnet-compatibility-") as temporary:
        scratch = Path(temporary)
        for version, revision in WRITERS.items():
            checkout = scratch / f"writer-v{version}"
            checkout.mkdir()
            # These archives share a package name/version and older source
            # mtimes. Never let Cargo reuse another writer's crate artifact.
            env = dict(os.environ, CARGO_TARGET_DIR=str(scratch / f"target-v{version}"))
            archive = subprocess.check_output(["git", "archive", revision], cwd=ROOT)
            with tarfile.open(fileobj=io.BytesIO(archive)) as files:
                files.extractall(checkout, filter="data")
            source = template
            if version >= 2:
                source = source.replace(
                    "// ADD_LABEL",
                    'database.execute("MATCH (n:Person {key:\'a\'}) SET n:Member")?;',
                )
            if version >= 4:
                source = source.replace(
                    "// ADD_CONSTRAINTS",
                    'database.create_unique_constraint("Person", "key")?;\n'
                    '    database.create_unique_constraint("Person", "number")?;',
                )
                source = source.replace("// MAKE_BACKUP", "database.backup_to(&fixture)?;")
            examples = checkout / "examples"
            examples.mkdir(exist_ok=True)
            (examples / "compatibility_fixture.rs").write_text(source)
            names = [f"v{version}.sqlite", f"v{version}.json"]
            command = [
                "cargo", "run", "--locked", "--release", "--example", "compatibility_fixture"
            ]
            if args.offline:
                command.append("--offline")
            command += ["--", str(version), *(str(output / name) for name in names)]
            print(f"Producing schema {version} with {revision}", flush=True)
            subprocess.run(command, cwd=checkout, env=env, check=True)
            manifest["fixtures"].append(
                {
                    "schema_version": version,
                    "writer_revision": revision,
                    "package_version": "0.1.0",
                    "kind": (
                        "public backup_to" if version >= 4
                        else "closed DELETE-journal file copy"
                    ),
                    "cargo_lock_sha256": sha256(checkout / "Cargo.lock"),
                    "compiled_producer_sha256": hashlib.sha256(source.encode()).hexdigest(),
                    "files_sha256": {name: sha256(output / name) for name in names},
                }
            )
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Fixtures written to {output}; inspect before retaining them.")


if __name__ == "__main__":
    main()
