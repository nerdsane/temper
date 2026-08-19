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

# `sandbox_url` is returned by the live API but is NOT declared in
# CreateSandboxResponse in the OpenAPI spec. Observed values:
#   ingress_endpoint = https://sandbox.tensorlake.ai              (SHARED host)
#   sandbox_url      = https://<id>.sandbox.tensorlake.ai         (per-sandbox)
# Proxy paths 404 with LIFECYCLE_PATH_NOT_FOUND on the shared host.
SBX_URL=$(printf '%s' "$CREATE_BODY_OUT" | jqp sandbox_url)
echo "initial sandbox_url=${SBX_URL:-<absent>}"

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

# ── 3. Proxy addressing: settle which base + headers actually route ──
#
# The shared ingress_endpoint rejects /api/v1/* with LIFECYCLE_PATH_NOT_FOUND.
# That error names two candidate routes, so probe BOTH:
#   (a) per-sandbox host  : $SBX_URL/api/v1/...        no routing header
#   (b) shared ingress    : $ING/api/v1/...            + x-tensorlake-sandbox-id
#
# Whichever returns a process event stream is the one the WASM modules use.
PROBE_CMD='{"command":"git --version; python3 --version; pip3 --version; echo PROBE_EXIT_OK","working_dir":"/"}'
PROXY_BASE=""
PROXY_MODE=""
PROXY_HDR=()

try_proxy() {
  # $1 = label, $2 = base url, $3... = extra curl args
  local label="$1"; local base="$2"; shift 2
  echo
  echo "  [$label] POST $base/api/v1/processes/run"
  local out
  out=$(curl -s -N -m 45 -w '\n__HTTP__%{http_code}' -X POST "$base/api/v1/processes/run" \
    -H "$AUTH" -H 'content-type: application/json' "$@" -d "$PROBE_CMD" || true)
  local code="${out##*__HTTP__}"
  local body="${out%__HTTP__*}"
  echo "  HTTP:$code"
  printf '%s\n' "$body" | head -40 | sed 's/^/    /'
  [ "$code" = "200" ]
}

echo
echo "--- 3. Proxy addressing (two candidates) ---"

if [ -n "$SBX_URL" ] && try_proxy "a: per-sandbox host" "$SBX_URL"; then
  PROXY_BASE="$SBX_URL"; PROXY_MODE="per-sandbox host (sandbox_url), no routing header"
elif try_proxy "b: shared ingress + routing header" "$ING" -H "x-tensorlake-sandbox-id: $SBX"; then
  PROXY_BASE="$ING"; PROXY_MODE="shared ingress_endpoint + x-tensorlake-sandbox-id header"
  PROXY_HDR=(-H "x-tensorlake-sandbox-id: $SBX")
fi

if [ -z "$PROXY_BASE" ]; then
  echo
  echo "FAIL: neither proxy addressing mode returned 200. Skipping file probe."
else
  echo
  echo "  => WORKING MODE: $PROXY_MODE"
  echo "  => base: $PROXY_BASE"

  # ── 4. Proxy: file write + read ────────────────────────────────────
  echo
  echo "--- 4. PUT + GET \$PROXY/api/v1/files?path=... ---"
  curl -s -o /dev/null -w 'PUT  HTTP:%{http_code}\n' \
    -X PUT "$PROXY_BASE/api/v1/files?path=/tmp/probe.txt" \
    -H "$AUTH" "${PROXY_HDR[@]:-}" --data-binary 'hello-from-probe'

  curl -s -w '\nGET  HTTP:%{http_code}\n' \
    "$PROXY_BASE/api/v1/files?path=/tmp/probe.txt" -H "$AUTH" "${PROXY_HDR[@]:-}"
fi

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
