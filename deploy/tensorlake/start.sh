#!/bin/bash
set -euo pipefail
# These files are installed after sandbox creation, never baked into an image.
set -a
source /etc/temper/runtime.env
set +a
export TURSO_URL=file:/var/lib/temper/agents.db
export TEMPER_LOCAL_BLOB_DIR=/var/lib/temper/blobs
export TEMPER_API_URL=http://127.0.0.1:3000
export AUTH_TRUST_HOST=true
export PORT=3001
export RUST_LOG=info
mkdir -p /var/lib/temper/blobs
pids=()
cleanup() {
  trap - EXIT TERM INT
  kill "${pids[@]}" 2>/dev/null || true
  wait || true
}
trap cleanup EXIT TERM INT
# Kernel timers are outside this deployment's supported contract; no wakeup service.
temper serve --storage turso --port 3000 &
pids+=("$!")
HOSTNAME=127.0.0.1 node /opt/temper/observe/server.js &
pids+=("$!")
nginx -g 'daemon off;' &
pids+=("$!")
wait -n "${pids[@]}"
# Restart the entire service group if any constituent exits.
exit 1
