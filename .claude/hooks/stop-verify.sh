#!/bin/bash
set -euo pipefail
cat > /dev/null
WORKSPACE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
echo "Running pre-exit workspace check..." >&2
if ! (cd "$WORKSPACE_ROOT" && cargo check --workspace 2>&1); then
    echo "WARNING: Workspace has compilation errors!" >&2
fi
exit 0
