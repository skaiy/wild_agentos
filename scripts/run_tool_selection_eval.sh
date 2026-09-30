#!/usr/bin/env bash
set -euo pipefail

cargo run --quiet --bin tool_selection_eval -- --offline --output target/tool-selection-eval
