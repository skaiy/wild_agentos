#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cargo test --workspace golden --verbose
./scripts/run_tool_selection_eval.sh
