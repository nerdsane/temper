#!/bin/bash
# verification-v1-validate.sh
# Validate that a JSON file conforms to the verification.v1 contract shape.
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: scripts/verification-v1-validate.sh <report.json>" >&2
    exit 2
fi

REPORT_FILE="$1"
if [ ! -f "$REPORT_FILE" ]; then
    echo "Report file not found: $REPORT_FILE" >&2
    exit 2
fi

jq -e '
def nonempty_string: (type == "string" and length > 0);
def nonneg_int: (type == "number" and floor == . and . >= 0);
def in01: (type == "number" and . >= 0 and . <= 1);
def valid_result: (. == "pass" or . == "fail" or . == "warn" or . == "skip" or . == "unknown");
def valid_stage:
    (. == "edit" or . == "commit" or . == "push" or . == "exit" or . == "review" or
     . == "trace" or . == "pow" or . == "git" or . == "config" or . == "ci" or
     . == "wiring" or . == "unknown");
def valid_evidence_class: (. == "mechanical" or . == "heuristic" or . == "attestation" or . == "inferred");

.schema == "verification.v1" and
(.run_id | nonempty_string) and
(.generated_at | nonempty_string) and

(.repository | type == "object") and
(.repository.path | nonempty_string) and
(.repository.project_hash | nonempty_string) and
(.repository.commit_head | nonempty_string) and
(.repository.branch | nonempty_string) and
(.repository.dirty | type == "boolean") and

(.agent | type == "object") and
(.agent.provider | nonempty_string) and
(.agent.model | nonempty_string) and
(.agent.session_id | nonempty_string) and

(.summary | type == "object") and
(.summary.overall_result | valid_result) and
(.summary.checks_total | nonneg_int) and
(.summary.checks_passed | nonneg_int) and
(.summary.checks_failed | nonneg_int) and
(.summary.checks_warned | nonneg_int) and
(.summary.checks_skipped | nonneg_int) and
(.summary.checks_unknown | nonneg_int) and
(.summary.blocking_failures | nonneg_int) and
(.summary.overall_hardness.accidental_regression | in01) and
(.summary.overall_hardness.adversarial_bypass | in01) and
(.summary.overall_hardness.portability | in01) and

(.checks | type == "array") and
(.summary.checks_total == (.checks | length)) and
(.summary.checks_passed == ([.checks[] | select(.result == "pass")] | length)) and
(.summary.checks_failed == ([.checks[] | select(.result == "fail")] | length)) and
(.summary.checks_warned == ([.checks[] | select(.result == "warn")] | length)) and
(.summary.checks_skipped == ([.checks[] | select(.result == "skip")] | length)) and
(.summary.checks_unknown == ([.checks[] | select(.result == "unknown")] | length)) and
(.summary.blocking_failures == ([.checks[] | select(.blocking == true and .result == "fail")] | length)) and
(
  all(.checks[];
      (.id | nonempty_string) and
      (.name | nonempty_string) and
      (.stage | valid_stage) and
      (.blocking | type == "boolean") and
      (.result | valid_result) and
      (.evidence_class | valid_evidence_class) and
      (.hardness.accidental_regression | in01) and
      (.hardness.adversarial_bypass | in01) and
      (.hardness.portability | in01) and
      (.detail | type == "string") and
      (.evidence.artifacts | type == "array") and
      all(.evidence.artifacts[]; nonempty_string)
  )
)
' "$REPORT_FILE" >/dev/null

echo "verification.v1 contract validation: OK ($REPORT_FILE)"
