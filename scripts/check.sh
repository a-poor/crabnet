#!/usr/bin/env bash
# The same complete checks run locally and on every CI matrix entry.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${CRABNET_TCK_UPDATE+x}" == x ]]; then
    echo 'Unset CRABNET_TCK_UPDATE: checks must verify, not rewrite, the TCK pins.' >&2
    exit 1
fi

export CRABNET_BENCH_SIZES=1000,10000
cargo test --locked --all-targets
cargo test --locked --doc
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
git diff --check
