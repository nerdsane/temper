#!/usr/bin/env bash
# PostToolUse hook: detect AuthorizationDenied in MCP temper execute results
# and auto-open the Observe UI decisions page.
#
# This hook matches the mcp__temper__execute tool and checks the result for
# AuthorizationDenied errors. When found, it opens the Observe UI and outputs
# decision details to the terminal.

set -euo pipefail

# Only process mcp__temper__execute tool results
TOOL_NAME="${CLAUDE_TOOL_NAME:-}"
if [[ "$TOOL_NAME" != "mcp__temper__execute" ]]; then
    exit 0
fi

# Read the tool result from stdin
RESULT=$(cat)

# Check if the result contains AuthorizationDenied
if ! echo "$RESULT" | grep -q "AuthorizationDenied"; then
    exit 0
fi

# Extract decision ID (format: PD-<uuid>)
DECISION_ID=$(echo "$RESULT" | grep -o 'PD-[a-zA-Z0-9_-]*' | head -1)

if [[ -n "$DECISION_ID" ]]; then
    echo ""
    echo "🔒 GOVERNANCE DECISION REQUIRED"
    echo "   Decision: $DECISION_ID"
    echo "   Status: Pending human approval"
    echo ""
    echo "   Approve via:"
    echo "     • Observe UI: http://localhost:3001/decisions"
    echo "     • Terminal:   temper decide --tenant <tenant>"
    echo ""

    # Auto-open the Observe UI decisions page on macOS
    if command -v open &>/dev/null; then
        open "http://localhost:3001/decisions" 2>/dev/null || true
    fi
fi

exit 0
