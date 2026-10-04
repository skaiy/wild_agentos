#!/usr/bin/env bash
set -euo pipefail

# Demo tenant helper (docs/28-demo-tenant.md): mint a tenant-scoped demo token,
# seed fictional sample data through the public HTTP API, and verify isolation.
# It never prints the signing secret or a token: tokens live only in mode-600
# files and reach curl through a header file, never through argv.

DEMO_TENANT="${DEMO_TENANT-demo}"
DEMO_PROJECT="${DEMO_PROJECT-showcase}"
DEMO_ACTOR="${DEMO_ACTOR:-demo-user}"
DEMO_ROLES="${DEMO_ROLES:-DA,mcp_invoke}"
PLATFORM_TENANT="${AGENTOS_PLATFORM_ADMIN_TENANT:-}"
BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"
DEMO_TOKEN_FILE="${DEMO_TOKEN_FILE:-./demo-tenant.jwt}"
AGENT_NAME="Example Co. demo agent"
KB_NAME="Example Co. sample knowledge"

die() { printf 'Error: %s\n' "$1" >&2; exit 2; }
MAX_EXP_DAYS=30
usage() {
    printf 'Usage: %s mint [--out PATH] [--exp-days N (1-%s)] | seed | verify\n' "$0" "$MAX_EXP_DAYS"
    printf 'Note: until #302 merges, distributed demo tokens must carry only mcp_invoke (DEMO_ROLES=mcp_invoke, no DA); a DA token used for seeding stays local, mode 600, and is deleted after use.\n'
}

# Reject unsafe scope before parsing a subcommand or creating any files.
for scope in "$DEMO_TENANT" "$DEMO_PROJECT"; do
    [[ "$scope" =~ ^[A-Za-z0-9_-]+$ ]] && [[ "$scope" != "." && "$scope" != ".." ]] ||
        die "DEMO_TENANT and DEMO_PROJECT must be simple, non-dot identifiers."
done
[[ "$DEMO_TENANT" != default ]] || die "DEMO_TENANT cannot be default."
[[ "$PLATFORM_TENANT" != default ]] || die "AGENTOS_PLATFORM_ADMIN_TENANT cannot be default."
[[ -z "$PLATFORM_TENANT" || "$DEMO_TENANT" != "$PLATFORM_TENANT" ]] ||
    die "DEMO_TENANT cannot equal AGENTOS_PLATFORM_ADMIN_TENANT."
if [[ -z "$PLATFORM_TENANT" ]]; then
    printf 'Note: AGENTOS_PLATFORM_ADMIN_TENANT is not set in this shell; cannot confirm the demo tenant differs from the platform tenant.\n' >&2
fi
IFS=, read -r -a roles <<< "$DEMO_ROLES"
for role in "${roles[@]}"; do
    role="${role//[[:space:]]/}"
    [[ "${role^^}" != PLATFORM_ADMIN ]] || die "DEMO_ROLES cannot contain PLATFORM_ADMIN."
done

require_secret() {
    python3 - <<'PY' || die "AGENTOS_JWT_SECRET must be at least 32 bytes."
import os
import sys
sys.exit(0 if len(os.environ.get("AGENTOS_JWT_SECRET", "").encode()) >= 32 else 1)
PY
}

# Mint directly to a protected file, never to shell output, argv, or another process's env.
mint_jwt() {
    local output="$1" days="$2" tenant="$3" summary="${4:-false}" role_string="${5:-$DEMO_ROLES}"
    python3 - "$output" "$days" "$tenant" "$summary" "$DEMO_PROJECT" "$DEMO_ACTOR" "$role_string" <<'PY'
import base64
import datetime
import hashlib
import hmac
import json
import os
import sys
import time

path, days, tenant, summary, project, actor, role_string = sys.argv[1:]
roles = [role.strip() for role in role_string.split(",") if role.strip()]
expiry = int(time.time()) + int(days) * 86400

def encode(value):
    return base64.urlsafe_b64encode(value).rstrip(b"=")

header = encode(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
claims = {"sub": actor, "tenant_id": tenant, "project_id": project,
          "roles": roles, "exp": expiry}
payload = encode(json.dumps(claims, separators=(",", ":")).encode())
signed = header + b"." + payload
signature = encode(hmac.new(os.environ["AGENTOS_JWT_SECRET"].encode(),
                            signed, hashlib.sha256).digest())
flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0)
if os.path.islink(path):
    raise SystemExit("Refusing to overwrite a symlink.")
fd = os.open(path, flags, 0o600)
with os.fdopen(fd, "wb") as out:
    out.write(signed + b"." + signature + b"\n")
    os.fchmod(out.fileno(), 0o600)
if summary == "true":
    print(path)
    print(f"tenant={tenant} project={project} actor={actor} roles={','.join(roles)} "
          f"exp={datetime.datetime.fromtimestamp(expiry, datetime.timezone.utc).isoformat()}")
PY
}

TMP_DIR=""
cleanup() {
    if [[ -n "$TMP_DIR" ]]; then
        rm -rf -- "$TMP_DIR"
    fi
}
trap cleanup EXIT

prepare_http() {
    [[ -f "$DEMO_TOKEN_FILE" && ! -L "$DEMO_TOKEN_FILE" ]] ||
        die "Demo token file missing or a symlink; run mint first."
    umask 077
    TMP_DIR="$(mktemp -d)"
    RESPONSE_BODY="$TMP_DIR/response.json"
    make_header "$DEMO_TOKEN_FILE" "$TMP_DIR/demo.header"
}

make_header() {
    python3 - "$1" "$2" <<'PY'
import os
import sys
with open(sys.argv[1], encoding="ascii") as token_file:
    token = token_file.read().strip()
if not token or "\n" in token or "\r" in token:
    raise SystemExit("Invalid token file.")
fd = os.open(sys.argv[2], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w", encoding="ascii") as header:
    header.write("Authorization: Bearer " + token + "\n")
PY
}

fetch() {
    local header="$1" method="$2" path="$3"
    shift 3
    HTTP_STATUS="$(curl --silent --show-error --path-as-is --connect-timeout 3 --max-time 30 \
        --request "$method" --header "@$header" --output "$RESPONSE_BODY" \
        --write-out '%{http_code}' "$@" "${BASE_URL%/}${path}")"
}

expect() {
    local status="$1"
    shift
    fetch "$@"
    [[ "$HTTP_STATUS" == "$status" ]]
}

field() {
    python3 - "$RESPONSE_BODY" "$1" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    value = json.load(source)
for key in sys.argv[2].split("."):
    value = value[key]
if not isinstance(value, str) or not value:
    raise SystemExit("Missing response field.")
print(value)
PY
}

# Print only an ID/name, never a response containing credentials.
named_id() {
    python3 - "$RESPONSE_BODY" "$1" "$2" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    items = json.load(source)[sys.argv[2]]
print(next((item["id"] for item in items if item.get("name") == sys.argv[3]), ""))
PY
}

document_exists() {
    python3 - "$RESPONSE_BODY" "$1" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    docs = json.load(source)["documents"]
sys.exit(0 if any(d.get("filename") == sys.argv[2] or d.get("name") == sys.argv[2]
                  for d in docs) else 1)
PY
}

check() {
    local label="$1"
    shift
    if "$@"; then
        printf 'PASS %s\n' "$label"
    else
        printf 'FAIL %s\n' "$label"
        FAIL=$((FAIL + 1))
    fi
}

# Reads only the role claim from the local token file; prints nothing.
token_has_role() {
    python3 - "$1" "$2" <<'PY'
import base64
import json
import sys
with open(sys.argv[1], encoding="ascii") as token_file:
    parts = token_file.read().strip().split(".")
try:
    payload = parts[1] + "=" * (-len(parts[1]) % 4)
    roles = json.loads(base64.urlsafe_b64decode(payload)).get("roles", [])
except (IndexError, ValueError):
    sys.exit(1)
sys.exit(0 if isinstance(roles, list) and sys.argv[2] in roles else 1)
PY
}

# Mirrors the kernel's scrub_secret_fields rule: a field is secret when its
# name, lowercased with `_`/`-` removed, ends with a secret word, unless it
# ends with `configured` (e.g. api_key_configured). Prints field paths only,
# never values.
config_has_no_secret_fields() {
    python3 - "$RESPONSE_BODY" <<'PY'
import json
import sys
SECRET = ("apikey", "secret", "token", "password", "secretkey", "privatekey",
          "accesskey", "authorization", "credential", "credentials")
def secret(key):
    name = "".join(c for c in key if c not in "_-").lower()
    return not name.endswith("configured") and name.endswith(SECRET)
found = []
def walk(value, path):
    if isinstance(value, dict):
        for key, child in value.items():
            child_path = f"{path}.{key}" if path else key
            if secret(key):
                found.append(child_path)
            walk(child, child_path)
    elif isinstance(value, list):
        for index, child in enumerate(value):
            walk(child, f"{path}[{index}]")
try:
    with open(sys.argv[1], encoding="utf-8") as source:
        walk(json.load(source), "")
except ValueError:
    print("config response is not JSON", file=sys.stderr)
    sys.exit(1)
if found:
    print("secret-looking fields present: " + ", ".join(found[:5]), file=sys.stderr)
    sys.exit(1)
PY
}

list_excludes_id() {
    expect 200 "$TMP_DIR/other.header" GET /api/v1/agents &&
        python3 - "$RESPONSE_BODY" "$1" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    agents = json.load(source)["agents"]
sys.exit(0 if all(agent.get("id") != sys.argv[2] for agent in agents) else 1)
PY
}

case "${1:-}" in
    --help|-h)
        usage
        ;;
    mint)
        shift
        out="./demo-tenant.jwt"
        days=7
        while (($#)); do
            case "$1" in
                --out) (($# >= 2)) || die "--out needs a path."; out="$2"; shift 2 ;;
                --exp-days) (($# >= 2)) || die "--exp-days needs a number."; days="$2"; shift 2 ;;
                *) die "Unknown mint argument: $1" ;;
            esac
        done
        [[ -n "$out" ]] || die "Provide a path for --out."
        # Validate before the secret is read or anything is signed.
        if ! [[ "$days" =~ ^[0-9]{1,6}$ ]] || ((10#$days < 1 || 10#$days > MAX_EXP_DAYS)); then
            die "--exp-days must be an integer from 1 to $MAX_EXP_DAYS."
        fi
        days=$((10#$days))
        require_secret
        umask 077
        mint_jwt "$out" "$days" "$DEMO_TENANT" true
        ;;
    seed)
        (($# == 1)) || die "seed takes no arguments."
        prepare_http
        expect 200 "$TMP_DIR/demo.header" GET /api/v1/agents || die "Cannot list demo agents."
        agent_id="$(named_id agents "$AGENT_NAME")"
        if [[ -z "$agent_id" ]]; then
            expect 201 "$TMP_DIR/demo.header" POST /api/v1/agents \
                --header 'Content-Type: application/json' \
                --data '{"name":"Example Co. demo agent","description":"Fictional sample agent"}' ||
                die "Cannot create demo agent."
            agent_id="$(field id)"
        fi
        printf 'Agent ready: %s\n' "$agent_id"
        expect 200 "$TMP_DIR/demo.header" GET /api/v1/kb/bases || die "Cannot list demo bases."
        kb_id="$(named_id bases "$KB_NAME")"
        if [[ -z "$kb_id" ]]; then
            expect 201 "$TMP_DIR/demo.header" POST /api/v1/kb/bases \
                --header 'Content-Type: application/json' \
                --data '{"name":"Example Co. sample knowledge","description":"Fictional sample documents","kb_type":"vector"}' ||
                die "Cannot create demo base."
            kb_id="$(field base.id)"
        fi
        printf 'Knowledge base ready: %s\n' "$kb_id"
        script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
        for fixture in "$script_dir"/examples/demo/*; do
            [[ -f "$fixture" ]] || die "No demo fixtures found."
            expect 200 "$TMP_DIR/demo.header" GET "/api/v1/kb/bases/$kb_id/documents" ||
                die "Cannot list demo documents."
            if document_exists "${fixture##*/}"; then
                printf 'Document already present: %s\n' "${fixture##*/}"
                continue
            fi
            expect 200 "$TMP_DIR/demo.header" POST "/api/v1/kb/bases/$kb_id/upload" \
                --form "file=@${fixture}" || die "Cannot upload demo document (embedding must be ready)."
            # Upload can return 200 even when a document could not be indexed.
            python3 - "$RESPONSE_BODY" "${fixture##*/}" <<'PY' ||
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    result = json.load(source)
sys.exit(0 if any(f.get("name") == sys.argv[2] and f.get("chunks", 0) > 0
                  and "persist_warning" not in f
                  for f in result.get("files", [])) else 1)
PY
                die "Demo document was not indexed."
            printf 'Document ready: %s\n' "${fixture##*/}"
        done
        ;;
    verify)
        (($# == 1)) || die "verify takes no arguments."
        require_secret
        prepare_http
        FAIL=0
        check "demo agents readable" expect 200 "$TMP_DIR/demo.header" GET /api/v1/agents
        if [[ "$HTTP_STATUS" == 200 ]]; then
            agent_id="$(named_id agents "$AGENT_NAME")"
        else
            agent_id=""
        fi
        check "demo knowledge bases readable" expect 200 "$TMP_DIR/demo.header" GET /api/v1/kb/bases
        check "demo cannot change global config" expect 403 "$TMP_DIR/demo.header" PUT /api/v1/config \
            --header 'Content-Type: application/json' --data '{"gateway":{}}'
        # Since #295 a control-plane DA token reads GET /api/v1/config (200) and
        # the kernel scrubs secret fields (scrub_secret_fields in config.rs).
        # A token without DA must still be refused.
        if ! fetch "$TMP_DIR/demo.header" GET /api/v1/config; then
            printf 'FAIL demo config read request failed\n'
            FAIL=$((FAIL + 1))
        elif token_has_role "$DEMO_TOKEN_FILE" DA; then
            if [[ "$HTTP_STATUS" != 200 ]]; then
                printf 'FAIL demo DA config read returned %s, expected 200\n' "$HTTP_STATUS"
                FAIL=$((FAIL + 1))
            else
                check "demo DA config read has no secret fields" config_has_no_secret_fields
            fi
        else
            case "$HTTP_STATUS" in
                401|403) printf 'PASS demo without DA cannot read global config\n' ;;
                *) printf 'FAIL demo without DA config read returned %s, expected 401/403\n' "$HTTP_STATUS"
                   FAIL=$((FAIL + 1)) ;;
            esac
        fi
        if [[ -z "$agent_id" ]]; then
            printf 'FAIL demo agent missing; run seed first\n'
            FAIL=$((FAIL + 1))
        else
            umask 077
            mint_jwt "$TMP_DIR/other.jwt" 1 "${DEMO_TENANT}-xcheck" false DA
            make_header "$TMP_DIR/other.jwt" "$TMP_DIR/other.header"
            check "other tenant cannot see demo agent" list_excludes_id "$agent_id"
            check "other tenant cannot update demo agent" expect 404 \
                "$TMP_DIR/other.header" PUT "/api/v1/agents/$agent_id" \
                --header 'Content-Type: application/json' --data '{"name":"not allowed"}'
        fi
        ((FAIL == 0))
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
