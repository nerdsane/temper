#!/bin/bash
set -euo pipefail
# Read stdin payload but we don't need to parse it
cat > /dev/null
# Check if .progress/ has any files
PROGRESS_DIR="$(dirname "$0")/../../.progress"
if [ -d "$PROGRESS_DIR" ] && ls "$PROGRESS_DIR"/*.md >/dev/null 2>&1; then
    echo "Reminder: Check .progress/ plan files before making changes." >&2
fi
exit 0
