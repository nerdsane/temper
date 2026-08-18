#!/bin/bash
# Issue Pickup Enforcement (PreToolUse — Write|Edit)
# ADVISORY: exit 0 always (never blocks), warns via stderr
#
# Checks for marker file that indicates an active Temper issue
# has been picked up for the current session. The marker is created
# when the agent transitions an issue to Planning or InProgress.
#
# Marker: /tmp/temper-harness/{project_hash}/{session_id}/issue-active
set -euo pipefail

# Consume stdin payload
cat > /dev/null

WORKSPACE_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
PROJECT_HASH="$(echo "$WORKSPACE_ROOT" | shasum -a 256 | cut -c1-12)"
SESSION_ID="${CLAUDE_SESSION_ID:-default}"
MARKER_DIR="/tmp/temper-harness/${PROJECT_HASH}/${SESSION_ID}"
MARKER_FILE="${MARKER_DIR}/issue-active"

if [ ! -f "$MARKER_FILE" ]; then
    echo "" >&2
    echo "── Advisory: No active Temper issue for this session ──" >&2
    echo "  Pick up or create a Temper issue before starting work." >&2
    echo "  Use BeginPlanning or StartWork to activate tracking." >&2
    echo "────────────────────────────────────────────────────────" >&2
fi

# Always advisory — never block
exit 0
