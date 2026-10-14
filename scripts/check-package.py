#!/usr/bin/env python3
"""Rehearse packaging and a clean offline consumer (Python 3.12+). Never publishes."""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--allow-dirty", action="store_true", help="development rehearsal only")
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
work = Path(tempfile.mkdtemp(prefix="crabnet-package-"))
print(f"Retained package evidence: {work}", flush=True)
commands = []

def run(command, cwd=root, env=None):
    print("Running:", " ".join(map(str, command)), flush=True)
    subprocess.run(command, cwd=cwd, env=env, check=True)
    commands.append({"argv": list(map(str, command)), "exit_code": 0})

def capture(command, cwd=root):
    return subprocess.check_output(command, cwd=cwd, text=True)

metadata = json.loads(capture(["cargo", "metadata", "--locked", "--offline", "--format-version", "1"]))
package = tomllib.loads((root / "Cargo.toml").read_text())["package"]
name = f'{package["name"]}-{package["version"]}'
run(["cargo", "package", "--locked", "--offline"] + (["--allow-dirty"] if args.allow_dirty else []))
archive = Path(metadata["target_directory"]) / "package" / f"{name}.crate"
shutil.copyfile(archive, work / archive.name)
with tarfile.open(archive) as tar:
    members = tar.getnames()
    for required in ["LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY.md", "README.md",
                     "tests/tck/vendor/openCypher-2024.3/LICENSE",
                     "tests/tck/vendor/openCypher-2024.3/NOTICE",
                     "examples/release_smoke.rs", "docs/dependency-inventory.json"]:
        if f"{name}/{required}" not in members:
            raise SystemExit(f"required archive member missing: {required}")
    tar.extractall(work, filter="data")
extracted = work / name
consumer = work / "consumer"
(consumer / "src").mkdir(parents=True)
(consumer / "Cargo.toml").write_text(
    '[package]\nname = "crabnet-package-consumer"\nversion = "0.0.0"\nedition = "2024"\npublish = false\n'
    + '\n[dependencies]\ncrabnet = { path = ' + json.dumps(str(extracted)) + ' }\nserde_json = \"1\"\n'
)
shutil.copyfile(extracted / "examples/release_smoke.rs", consumer / "src/main.rs")
shutil.copyfile(extracted / "Cargo.lock", consumer / "Cargo.lock")
readme_examples = re.findall(r"```rust\n(.*?)```", (extracted / "README.md").read_text(), re.S)
if not readme_examples:
    raise SystemExit("no Rust README examples found")
(consumer / "src/bin").mkdir()
for index, code in enumerate(readme_examples):
    code = "\n".join(line[2:] if line.startswith("# ") else "" if line == "#" else line for line in code.splitlines())
    if "fn main(" not in code:
        code = "fn main() -> crabnet::Result<()> {\n" + code + "\n}\n"
    (consumer / "src/bin" / f"readme_{index}.rs").write_text(code)

# Admit only the new consumer root to the lockfile before the locked build.
resolved = json.loads(capture(["cargo", "metadata", "--offline", "--format-version", "1"], consumer))
expected = {(p["name"], p["version"], p["source"]) for p in metadata["packages"]}
for dependency in resolved["packages"]:
    if dependency["name"] == "crabnet-package-consumer":
        continue
    if (dependency["name"], dependency["version"], dependency["source"]) not in expected:
        raise SystemExit(f"consumer changed locked dependency: {dependency['name']}")
    if dependency["name"] == "crabnet" and Path(dependency["manifest_path"]).parent != extracted:
        raise SystemExit("consumer resolved Crabnet outside the unpacked archive")
env = dict(os.environ, CARGO_TARGET_DIR=str(work / "target"))
run(["cargo", "check", "--locked", "--offline", "--bins"], consumer, env)
run(["cargo", "run", "--locked", "--offline", "--bin", "crabnet-package-consumer", "--", str(work / "consumer-data")], consumer, env)
run(["cargo", "test", "--locked", "--offline", "--doc"], extracted, env)
run(["cargo", "test", "--locked", "--offline", "--test", "tck", "--test", "storage_compatibility", "--test", "transactions"], extracted, env)
report = {
    "development_rehearsal": args.allow_dirty,
    "source_commit": capture(["git", "rev-parse", "HEAD"]).strip(),
    "source_status": capture(["git", "status", "--porcelain"]),
    "rustc": capture(["rustc", "-Vv"]),
    "cargo": capture(["cargo", "-V"]).strip(),
    "archive": archive.name,
    "archive_sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
    "archive_bytes": archive.stat().st_size,
    "archive_members": len(members),
    "readme_examples_compiled": len(readme_examples),
    "lockfile_sha256": hashlib.sha256((extracted / "Cargo.lock").read_bytes()).hexdigest(),
    "commands": commands,
}
(work / "evidence.json").write_text(json.dumps(report, indent=2) + "\n")
print(f"Package rehearsal passed. Evidence: {work / 'evidence.json'}", flush=True)
