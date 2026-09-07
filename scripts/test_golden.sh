#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
exec cargo test --workspace golden --verbose
