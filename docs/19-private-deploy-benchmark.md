# Private deployment benchmark

This benchmark measures the single-node, CPU-only storage profile for Oxigraph,
redb, and Hyperspace HNSW. It is intended for private or edge deployments where
model slots may degrade to locally available CPU-capable models. It does not
invoke a model, require a GPU, deploy anything, or use credentials.

## Run

```bash
./scripts/bench_private_deploy.sh
```

The command recreates its output directory, runs a fixed deterministic dataset,
and writes both machine-readable and reviewable reports:

- `target/private-deploy-bench/report.json`
- `target/private-deploy-bench/report.md`

For CI or a quicker local smoke run, use the smaller fixed `ci-micro` profile:

```bash
./scripts/bench_private_deploy.sh --micro
```

To retain reports elsewhere under `target/`, set `PRIVATE_BENCH_OUTPUT_DIR`.
The script rejects paths outside `target/`, because it clears the output
directory before each run.

## What it measures

The normal profile uses `synthetic-deterministic-v1`: 10,000 RDF records,
10,000 redb keys, and 10,000 deterministic 64-dimensional vectors. It records
100 samples each for:

| Store | Metric |
|---|---|
| Oxigraph | SPARQL `SELECT` latency |
| redb | KV `get` latency |
| Hyperspace | HNSW top-10 search latency |

The JSON report also includes p50/p95/p99/min/max latency in microseconds,
on-disk bytes per store and total, process RSS/peak RSS, OS, architecture,
logical CPU count, CPU model when the runner exposes it, and total RAM.

## Reporting rule

The README performance table is historical reference only. Do not alter it,
publish a speedup ratio, or claim a performance improvement based on this
document or an unsourced run. Any performance statement must link to the
corresponding committed `report.json` (or another durable, sourced measured
run) and identify its machine and profile. CI runs are record-only because
shared CI machines are not stable performance baselines.
