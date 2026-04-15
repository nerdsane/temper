#!/usr/bin/env bash
set -euo pipefail

# Configure branch protection for mainline CI enforcement.
# Usage:
#   scripts/set-branch-protection.sh [owner/repo] [branch]
# Example:
#   scripts/set-branch-protection.sh nerdsane/temper main

REPO="${1:-nerdsane/temper}"
BRANCH="${2:-main}"

if ! command -v gh >/dev/null 2>&1; then
    echo "ERROR: gh CLI is required." >&2
    exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
    echo "ERROR: gh auth is not valid. Run: gh auth login -h github.com" >&2
    exit 1
fi

echo "Applying branch protection to ${REPO}:${BRANCH} ..."

gh api \
    --method PUT \
    -H "Accept: application/vnd.github+json" \
    "repos/${REPO}/branches/${BRANCH}/protection" \
    --input - <<'JSON'
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "Verification Contract (verification.v1)",
      "Compile & Lint",
      "Integrity & DST Patterns",
      "Tests",
      "Spec Verification (L0-L3)"
    ]
  },
  "enforce_admins": true,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": false,
    "required_approving_review_count": 1
  },
  "required_conversation_resolution": true,
  "restrictions": null
}
JSON

echo "Branch protection applied successfully."
