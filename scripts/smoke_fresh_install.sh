#!/usr/bin/env bash
# Fresh-install smoke test (#277).
#
# Starts the Core binary with the repository's own config.yaml, empty temporary
# data directories and environment variables only, waits for GET /health to
# return 200, then checks that no legacy shared L0 database (l0.redb) was
# created and shuts the server down.
#
# Usage: scripts/smoke_fresh_install.sh [BINARY]
#   BINARY defaults to $AGENTOS_SMOKE_BIN, then target/release/wild-agent-os-core.
# No real gateway key is used: gateway.base_url/api_key stay empty (warn only),
# and the HS256 JWT secret is random throwaway material generated here.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-${AGENTOS_SMOKE_BIN:-$REPO_ROOT/target/release/wild-agent-os-core}}"
TIMEOUT_SECS="${AGENTOS_SMOKE_TIMEOUT_SECS:-120}"

if [[ ! -x "$BIN" ]]; then
  echo "smoke: binary not found or not executable: $BIN" >&2
  exit 2
fi
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
    for _ in $(seq 1 30); do
      kill -0 "$SERVER_PID" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

L0_DIR="$WORK/l0"
HTTP_PORT="$(free_port)"
GRPC_PORT="$(free_port)"
LOG="$WORK/server.log"
mkdir -p "$WORK/home"

# config.yaml is read from the working directory (repository root) unchanged.
# memory.l0.path has no explicit env mapping, so it is redirected to the empty
# temp directory through the generic AGENT_OS_<SECTION>_<FIELD> fallback.
cd "$REPO_ROOT"
env -i \
  PATH="$PATH" \
  HOME="$WORK/home" \
  RUST_LOG="${RUST_LOG:-info}" \
  AGENTOS_JWT_SECRET="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')" \
  AGENTOS_DATA_DIR="$WORK/data" \
  AGENT_OS_HTTP_PORT="$HTTP_PORT" \
  AGENT_OS_API_GRPC_ADDR="127.0.0.1:$GRPC_PORT" \
  AGENT_OS_OUTPUT_DIRECTORY="$WORK/output" \
  AGENT_OS_MEMORY_L0_PATH="$L0_DIR" \
  "$BIN" >"$LOG" 2>&1 &
SERVER_PID=$!

deadline=$((SECONDS + TIMEOUT_SECS))
status=""
while (( SECONDS < deadline )); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "smoke: server exited before /health became ready" >&2
    tail -n 50 "$LOG" >&2
    exit 1
  fi
  status="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:$HTTP_PORT/health" || true)"
  [[ "$status" == "200" ]] && break
  sleep 1
done

if [[ "$status" != "200" ]]; then
  echo "smoke: /health did not return 200 within ${TIMEOUT_SECS}s (last status: ${status:-none})" >&2
  tail -n 50 "$LOG" >&2
  exit 1
fi

if [[ -e "$L0_DIR/l0.redb" ]]; then
  echo "smoke: startup created a legacy shared L0 database at $L0_DIR/l0.redb" >&2
  exit 1
fi

echo "smoke: OK — /health 200 on fresh install, no legacy l0.redb created"
