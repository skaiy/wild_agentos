#!/usr/bin/env bash
set -euo pipefail

cargo run --quiet --bin tool-selection-eval -- --offline --output target/tool-selection-eval
