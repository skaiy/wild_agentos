#!/usr/bin/env bash
set -euo pipefail

# Exercises the HTTP isolation boundary of an already-running local kernel.
# It intentionally does not start a server or supply a default JWT secret.

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"
AGENTOS_JWT_SECRET="${AGENTOS_JWT_SECRET:-}"
PASS=0
FAIL=0
TMP_DIR="$(mktemp -d)"
RESPONSE_BODY="$TMP_DIR/response.json"
HTTP_STATUS=""

cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

green() { printf '\033[32m%s\033[0m\n' "$1"; }
red() { printf '\033[31m%s\033[0m\n' "$1"; }
bold() { printf '\033[1m%s\033[0m\n' "$1"; }

header() {
    bold "━━━ $1 ━━━"
}

check() {
    local label="$1"
    shift
    if "$@"; then
        green "  ✓ $label"
        PASS=$((PASS + 1))
    else
        red "  ✗ $label"
        FAIL=$((FAIL + 1))
    fi
}

require_secret() {
    if [ -z "$AGENTOS_JWT_SECRET" ]; then
        red "AGENTOS_JWT_SECRET is required (at least 32 bytes)."
        exit 2
    fi
    if ! python3 -c 'import os, sys; sys.exit(len(os.environ["AGENTOS_JWT_SECRET"].encode()) >= 32)' 2>/dev/null; then
        red "AGENTOS_JWT_SECRET must be at least 32 bytes."
        exit 2
    fi
}

mint_jwt() {
    local tenant_id="$1"
    python3 - "$tenant_id" <<'PY'
import base64
import hashlib
import hmac
import json
import os
import sys
import time

def encode(value):
    return base64.urlsafe_b64encode(value).rstrip(b"=")

header = encode(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
claims = encode(json.dumps({
    "sub": f"claims-smoke-{sys.argv[1]}",
    "tenant_id": sys.argv[1],
    "project_id": "project-a",
    "exp": int(time.time()) + 300,
}, separators=(",", ":")).encode())
signed = header + b"." + claims
signature = encode(hmac.new(
    os.environ["AGENTOS_JWT_SECRET"].encode(),
    signed,
    hashlib.sha256,
).digest())
print((signed + b"." + signature).decode())
PY
}

fetch() {
    local method="$1"
    local path="$2"
    local token="${3:-}"
    local payload="${4:-}"
    local -a args=(
        --silent --show-error --path-as-is
        --connect-timeout 3 --max-time 20
        --request "$method"
        --output "$RESPONSE_BODY"
        --write-out '%{http_code}'
    )

    if [ -n "$token" ]; then
        args+=(--header "Authorization: Bearer $token")
    fi
    if [ -n "$payload" ]; then
        args+=(--header 'Content-Type: application/json' --data "$payload")
    fi

    HTTP_STATUS="$(curl "${args[@]}" "${BASE_URL%/}${path}")"
}

expect_status() {
    local expected="$1"
    shift
    fetch "$@" && [ "$HTTP_STATUS" = "$expected" ]
}

json_field() {
    local field="$1"
    python3 - "$field" <"$RESPONSE_BODY" <<'PY'
import json
import sys

value = json.load(sys.stdin)
for part in sys.argv[1].split("."):
    value = value[part]
if not isinstance(value, str) or not value:
    raise ValueError("expected a non-empty string")
print(value)
PY
}

json_list_contains() {
    local list_field="$1"
    local needle="$2"
    python3 - "$list_field" "$needle" <"$RESPONSE_BODY" <<'PY'
import json
import sys

document = json.load(sys.stdin)
items = document[sys.argv[1]]
needle = sys.argv[2]
sys.exit(0 if any(
    isinstance(item, dict)
    and any(item.get(key) == needle for key in ("id", "iri", "task_iri"))
    for item in items
) else 1)
PY
}

body_does_not_contain() {
    local needle="$1"
    ! python3 -c 'import sys; sys.exit(0 if sys.argv[1] in sys.stdin.read() else 1)' \
        "$needle" <"$RESPONSE_BODY"
}

url_path_segment() {
    python3 - "$1" <<'PY'
import sys
import urllib.parse
print(urllib.parse.quote(sys.argv[1], safe=""))
PY
}

require_secret
TOKEN_A="$(mint_jwt tenant-a)"
TOKEN_B="$(mint_jwt tenant-b)"

header "Authentication"
check "Anonymous GET /api/v1/agents is rejected" expect_status 401 GET /api/v1/agents
check "Anonymous GET /api/v1/tasks is rejected" expect_status 401 GET /api/v1/tasks
check "Anonymous GET /api/v1/tasks/trends is rejected" expect_status 401 GET /api/v1/tasks/trends

header "Agents"
AGENT_PAYLOAD='{"name":"claims smoke agent","description":"Local isolation smoke test"}'
check "Tenant A creates a user agent" expect_status 201 POST /api/v1/agents "$TOKEN_A" "$AGENT_PAYLOAD"
AGENT_ID="$(json_field id)"
check "Tenant A lists its agent" expect_status 200 GET /api/v1/agents "$TOKEN_A"
check "Tenant A agent list contains the created id" json_list_contains agents "$AGENT_ID"
check "Tenant B lists agents" expect_status 200 GET /api/v1/agents "$TOKEN_B"
check "Tenant B agent list excludes Tenant A's id" body_does_not_contain "$AGENT_ID"
check "Tenant B cannot update Tenant A's agent" expect_status 404 PUT "/api/v1/agents/$AGENT_ID" "$TOKEN_B" '{"name":"forbidden"}'
check "Tenant B update response omits Tenant A's id" body_does_not_contain "$AGENT_ID"
check "Tenant B cannot delete Tenant A's agent" expect_status 404 DELETE "/api/v1/agents/$AGENT_ID" "$TOKEN_B"
check "Tenant B delete response omits Tenant A's id" body_does_not_contain "$AGENT_ID"

header "Tasks"
check "Tenant A creates a task" expect_status 201 POST /api/v1/tasks "$TOKEN_A" '{"user_input":"claims smoke task"}'
TASK_IRI="$(json_field task_iri)"
TASK_PATH="$(url_path_segment "$TASK_IRI")"
check "Tenant A reads its task detail" expect_status 200 GET "/api/v1/tasks/$TASK_PATH" "$TOKEN_A"
check "Tenant A reads its task status" expect_status 200 GET "/api/v1/tasks/$TASK_PATH/status" "$TOKEN_A"
check "Tenant A reads its task execution details" expect_status 200 GET "/api/v1/tasks/$TASK_PATH/details" "$TOKEN_A"
check "Tenant B cannot read Tenant A's task detail" expect_status 404 GET "/api/v1/tasks/$TASK_PATH" "$TOKEN_B"
check "Tenant B task detail response omits Tenant A's IRI" body_does_not_contain "$TASK_IRI"
check "Tenant A lists tasks" expect_status 200 GET /api/v1/tasks "$TOKEN_A"
check "Tenant A task list contains the created IRI" json_list_contains tasks "$TASK_IRI"
check "Tenant B lists tasks" expect_status 200 GET /api/v1/tasks "$TOKEN_B"
check "Tenant B task list excludes Tenant A's IRI" body_does_not_contain "$TASK_IRI"

echo ""
bold "═══════════════════════════════════════════"
bold "  Results: $PASS passed, $FAIL failed"
bold "═══════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
