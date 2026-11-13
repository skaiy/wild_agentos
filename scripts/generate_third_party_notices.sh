#!/usr/bin/env bash
# Regenerate THIRD_PARTY_NOTICES.md from the workspace graph.
# CI runs this and fails if the committed file differs (`git diff --exit-code`).

set -euo pipefail
cd "$(dirname "$0")/.."

tmp="$(mktemp)"
graph="$(mktemp)"
trap 'rm -f "$tmp" "$graph"' EXIT

cargo about generate --locked --workspace --fail --format json >"$graph"
cargo about generate --locked --workspace --fail about.hbs >"$tmp"
python3 scripts/append_apache_notices.py "$tmp" "$graph"
mv "$tmp" THIRD_PARTY_NOTICES.md
trap - EXIT
