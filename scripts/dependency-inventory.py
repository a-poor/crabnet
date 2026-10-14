#!/usr/bin/env python3
"""Record locked Cargo declarations and source-file hashes; no legal classification."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--check", action="store_true", help="verify the retained inventory without writing")
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=root
))
nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
root_id = metadata["resolve"]["root"]
runtime = set()
pending = [root_id]
while pending:
    key = pending.pop()
    if key in runtime:
        continue
    runtime.add(key)
    for dep in nodes[key]["deps"]:
        if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]):
            pending.append(dep["pkg"])
lock_bytes = (root / "Cargo.lock").read_bytes()
lock = tomllib.loads(lock_bytes.decode())
checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
records = []
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["id"] == root_id:
        continue
    directory = Path(package["manifest_path"]).parent
    texts = []
    for path in sorted(directory.iterdir()):
        if path.is_file() and any(word in path.name.lower() for word in ("license", "notice", "copying", "copyright")):
            texts.append({"path": path.name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
    records.append({
        "name": package["name"], "version": package["version"],
        "license": package["license"], "source": package["source"],
        "checksum": checksums[(package["name"], package["version"])],
        "scope": "runtime/build (including target-specific dependencies)" if package["id"] in runtime else "development only",
        "license_files": texts,
    })
report = {
    "format_version": 1,
    "lockfile_sha256": hashlib.sha256(lock_bytes).hexdigest(),
    "scope": "Cargo.lock resolved packages for all targets and enabled features; not a binary bill of materials or license-compliance certification. Dependency source archives are fetched separately by Cargo, not vendored in the Crabnet source package.",
    "packages": records,
}
rendered = json.dumps(report, indent=2) + "\n"
destination = root / "docs/dependency-inventory.json"
if args.check:
    if destination.read_text() != rendered:
        raise SystemExit("dependency inventory differs; regenerate and review it")
    print(f"verified {len(records)} dependency records")
else:
    destination.write_text(rendered)
    print(f"wrote {len(records)} dependency records")
