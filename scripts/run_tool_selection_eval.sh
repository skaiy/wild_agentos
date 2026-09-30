#!/usr/bin/env bash
set -euo pipefail

cargo run --quiet --bin tool_selection_eval -- \
  --offline \
  --compare evals/golden/tool-selection.baseline.json \
  --output target/tool-selection-eval
