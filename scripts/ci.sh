#!/usr/bin/env bash
# Local CI gate: the whole governance stack in one command.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
./scripts/check-boundaries.sh
./scripts/check-notes.sh
echo "CI: all gates green"
