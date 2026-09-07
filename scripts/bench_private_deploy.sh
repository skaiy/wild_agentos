#!/usr/bin/env bash
# Reproducible private single-node benchmark. See docs/19-private-deploy-benchmark.md.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

profile_args=()
if [[ "${1:-}" == "--micro" ]]; then
  profile_args+=(--micro)
  shift
fi
if [[ $# -ne 0 ]]; then
  echo "Usage: $0 [--micro]" >&2
  exit 64
fi

output_dir="${PRIVATE_BENCH_OUTPUT_DIR:-target/private-deploy-bench}"
case "$output_dir" in
  target/*) ;;
  *)
    echo "PRIVATE_BENCH_OUTPUT_DIR must be below target/ to prevent accidental deletion." >&2
    exit 64
    ;;
esac

rm -rf -- "$output_dir"
cargo run --release --bin private-deploy-bench -- --output "$output_dir" "${profile_args[@]}"
echo "Reports written to $output_dir/report.json and $output_dir/report.md"
