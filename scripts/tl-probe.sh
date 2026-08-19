#!/usr/bin/env bash
# Tensorlake API contract probe.
#
# Validates the exact Tensorlake sandbox API shape against a real API key
# BEFORE the WASM integration is rewritten against it. Every path and field
# below comes from the OpenAPI spec at
# https://docs.tensorlake.ai/api-reference/openapi.yaml
#
# Control plane (https://api.tensorlake.ai, Authorization: Bearer):
#   POST   /sandboxes                    create
#   GET    /sandboxes/{id}               status (poll)
#   POST   /sandboxes/{id}/snapshot      checkpoint -> 202 {snapshot_id,status}
#   DELETE /sandboxes/{id}               destroy
#
# Sandbox proxy (at the sandbox's own ingress_endpoint, also Bearer):
#   POST /api/v1/processes/run           SSE event stream (text/event-stream)
#   GET  /api/v1/files?path=...          read  (octet-stream)
#   PUT  /api/v1/files?path=...          write (raw body)
#
# Two facts that make polling mandatory:
#   - CreateSandboxResponse.ingress_endpoint is nullable (type: [string, null])
#   - status starts as `pending` with a pending_reason
# You cannot address the proxy until status == running AND ingress is non-null.
#
# Usage:
#   export TENSORLAKE_API_KEY="tlk_..."
#   ./scripts/tl-probe.sh
#
# Exit codes: 0 = probe completed, 1 = setup/create/boot failure.

set -euo pipefail

: "${TENSORLAKE_API_KEY:?export TENSORLAKE_API_KEY first}"

API="${TENSORLAKE_API_URL:-https://api.tensorlake.ai}"
AUTH="authorization: Bearer ${TENSORLAKE_API_KEY}"
POLL_ATTEMPTS="${POLL_ATTEMPTS:-60}"
POLL_INTERVAL="${POLL_INTERVAL:-2}"

# Optional: pin an image. Left empty so Tensorlake uses its default managed
# environment (per CreateSandboxRequest.image: "When omitted, Tensorlake uses
# the default managed environment").
SANDBOX_IMAGE="${SANDBOX_IMAGE:-}"

SBX=""

# Extract one top-level key from a JSON object on stdin. Prints empty string
# for null or missing so `[ -n ... ]` checks behave.
jqp() {
  python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
v = d.get('$1')
print('' if v is None else v)
"
}

cleanup() {
  if [ -n "$SBX" ]; then
    echo
    echo "--- cleanup: DELETE /sandboxes/$SBX ---"
    curl -s -o /dev/null -w 'DELETE HTTP:%{http_code}\n' \
      -X DELETE "$API/sandboxes/$SBX" -H "$AUTH" || true
  fi
}
trap cleanup EXIT

echo "=== Tensorlake API contract probe ==="
echo "  api:   $API"
echo "  image: ${SANDBOX_IMAGE:-<platform default>}"
echo

# ── 1. Create ────────────────────────────────────────────────────────
# Body shape per CreateSandboxRequest: resources and network are NESTED
# objects, not flat fields.
echo "--- 1. POST /sandboxes ---"

if [ -n "$SANDBOX_IMAGE" ]; then
  CREATE_BODY=$(printf '{"image":"%s","resources":{"cpus":1,"memory_mb":2048},"timeout_secs":900,"network":{"allow_internet_access":true}}' "$SANDBOX_IMAGE")
else
  CREATE_BODY='{"resources":{"cpus":1,"memory_mb":2048},"timeout_secs":900,"network":{"allow_internet_access":true}}'
fi

echo "request body: $CREATE_BODY"
CREATE=$(curl -s -w '\n__HTTP__%{http_code}' -X POST "$API/sandboxes" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d "$CREATE_BODY")

CREATE_CODE="${CREATE##*__HTTP__}"
CREATE_BODY_OUT="${CREATE%__HTTP__*}"
echo "HTTP:$CREATE_CODE"
echo "$CREATE_BODY_OUT"

SBX=$(printf '%s' "$CREATE_BODY_OUT" | jqp sandbox_id)
if [ -z "$SBX" ]; then
  echo
  echo "FAIL: no sandbox_id in response — nothing further to probe."
  echo "      401 => bad key. 404 => wrong path. 4xx with body => bad payload shape."
  exit 1
fi
echo "sandbox_id=$SBX"
echo "initial status=$(printf '%s' "$CREATE_BODY_OUT" | jqp status)"
echo "initial ingress_endpoint=$(printf '%s' "$CREATE_BODY_OUT" | jqp ingress_endpoint || true)"

# ── 2. Poll until running with a non-null ingress ────────────────────
echo
echo "--- 2. GET /sandboxes/$SBX (poll until running + ingress non-null) ---"
ING=""
BOOT_START=$(date +%s)
for i in $(seq 1 "$POLL_ATTEMPTS"); do
  INFO=$(curl -s "$API/sandboxes/$SBX" -H "$AUTH")
  ST=$(printf '%s' "$INFO" | jqp status)
  ING=$(printf '%s' "$INFO" | jqp ingress_endpoint)
  PR=$(printf '%s' "$INFO" | jqp pending_reason)
  echo "  [$i] status=${ST:-<none>} pending_reason=${PR:-<none>} ingress=${ING:-<null>}"

  if [ "$ST" = "running" ] && [ -n "$ING" ]; then
    BOOT_ELAPSED=$(( $(date +%s) - BOOT_START ))
    echo "  -> running with ingress after ${BOOT_ELAPSED}s"
    break
  fi

  case "$ST" in
    terminated|"")
      echo "  gave up. last response: $INFO"
      break
      ;;
  esac

  sleep "$POLL_INTERVAL"
done

if [ -z "$ING" ]; then
  echo
  echo "FAIL: never observed a non-null ingress_endpoint."
  exit 1
fi

echo "ingress_endpoint=$ING"

# ── 3. Proxy: run a process (SSE) ────────────────────────────────────
# Confirms three unknowns at once:
#   (a) the proxy accepts the same bearer token
#   (b) the exact SSE frame shape (data: {line,...} / data: {exit_code})
#   (c) whether the default image ships git + python3, which the repo clone
#       and the fixture tests both require
echo
echo "--- 3. POST \$ING/api/v1/processes/run  (expect text/event-stream) ---"
echo "request body: {\"command\":\"...\",\"working_dir\":\"/\"}   (note: working_dir, not workdir/cwd)"
curl -s -N -w '\n__HTTP__%{http_code}\n' -X POST "$ING/api/v1/processes/run" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d '{"command":"git --version; python3 --version; pip3 --version; echo PROBE_EXIT_OK","working_dir":"/"}' \
  | head -60

# ── 4. Proxy: file write + read ──────────────────────────────────────
echo
echo "--- 4. PUT + GET \$ING/api/v1/files?path=... ---"
curl -s -o /dev/null -w 'PUT  HTTP:%{http_code}\n' \
  -X PUT "$ING/api/v1/files?path=/tmp/probe.txt" \
  -H "$AUTH" --data-binary 'hello-from-probe'

curl -s -w '\nGET  HTTP:%{http_code}\n' \
  "$ING/api/v1/files?path=/tmp/probe.txt" -H "$AUTH"

# ── 5. Snapshot (checkpoint) ─────────────────────────────────────────
# Documented as 202 with {snapshot_id, status}. Note the path is singular
# /snapshot, not /snapshots.
echo
echo "--- 5. POST /sandboxes/$SBX/snapshot (expect 202) ---"
curl -s -w '\nHTTP:%{http_code}\n' -X POST "$API/sandboxes/$SBX/snapshot" \
  -H "$AUTH" -H 'content-type: application/json' \
  -d '{"snapshot_type":"filesystem"}'

echo
echo "=== probe complete (cleanup runs on exit) ==="
