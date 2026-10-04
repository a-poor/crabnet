#!/usr/bin/env python3
"""Run one already-built example process and add wall time and peak RSS to its JSON."""

import argparse
import hashlib
import json
import platform
import subprocess
import sys
import time
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if sys.platform not in ("darwin", "linux"):
        parser.error("peak-RSS measurement supports macOS and Linux")
    import resource

    command = [str(args.binary.resolve()), *args.arguments]
    started = time.perf_counter()
    child = subprocess.run(command, capture_output=True, text=True, check=False)
    elapsed = time.perf_counter() - started
    # Exactly one child: build separately so Cargo/rustc memory cannot pollute RSS.
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    if child.returncode:
        sys.stderr.write(child.stderr)
        sys.exit(child.returncode if child.returncode > 0 else 1)
    report = json.loads(child.stdout)
    with args.binary.open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    report["measurement"] = {
        "elapsed_seconds": elapsed,
        "peak_rss_bytes": int(peak if sys.platform == "darwin" else peak * 1024),
        "platform": platform.platform(),
        "architecture": platform.machine(),
        "binary_sha256": digest,
        "command": command,
    }
    if child.stderr:
        sys.stderr.write(child.stderr)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
