# Fix PoW: Trace Concurrency + Agent-Generated Claims

## Context

Two architectural issues in the Proof of Work system:
1. Trace hash chain breaks from concurrent agents (non-atomic read-modify-write)
2. Claims mechanically extracted from trace, making comparison tautological

## Phases

### Phase 1: Atomic Trace Writes — COMPLETE
- Added mkdir-based lock to trace-capture.sh
- Spin-wait with 2s timeout, skip trace rather than block agent
- Verified: 5 concurrent writers, 0 broken hash chain entries

### Phase 2: Agent-Generated Claims — COMPLETE
- Renamed pow-produce-claims.sh → pow-extract-evidence.sh (ground truth for debugging)
- Created pow-agent-claims.sh (agent self-reports claims)
- Updated pow-compare.sh, pre-commit-review-gate.sh, stop-verify.sh references
- Updated alignment-reviewer.md with agent-generated claims note
- Updated docs/HARNESS.md (components, verification flow, scripts, portability)

## Status: COMPLETE
