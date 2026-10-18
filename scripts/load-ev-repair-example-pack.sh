#!/usr/bin/env bash
# Validate the EV-repair example pack (filesystem gate). Optional runtime import
# for the 10-minute governed-ontology demo (#416) is not wired here yet.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
exec ./scripts/check_ev_repair_example_pack.sh
