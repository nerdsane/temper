# P-001: Agents Need Better CancelOrder Guidance

**Type:** P-Record (Problem)
**Timestamp:** 2025-01-15T15:00:00Z
**Severity:** Medium
**Origin:** [O-001](../observations/O-001-cancel-success-rate.md)

## Problem Statement

Production agents are attempting CancelOrder from states where cancellation is
not permitted (Processing, Shipped, Delivered). This results in a 27% failure
rate, causing poor user experience and wasted API calls.

The root cause is insufficient guidance in the CSDL metadata. The
`Agent.Hint` annotation on CancelOrder says "Only from Draft, Submitted, or
Confirmed" but agents are not consistently reading this before attempting the
action.

## Impact

- 229 failed CancelOrder attempts per week
- Each failure triggers a confusing error message to the end user
- Agents then retry with InitiateReturn, adding latency

## Constraints

- Cannot change the state machine (cancellation from Processing+ is not safe)
- Must be backwards-compatible with existing agents
- Should not require agent code changes

## Related

- **A-Record:** [A-001](../analyses/A-001-enhanced-cancel-hints.md)
