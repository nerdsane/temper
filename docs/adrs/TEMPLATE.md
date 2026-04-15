# ADR-NNNN: Title

- Status: Proposed | Accepted | Superseded | Deprecated
- Date: YYYY-MM-DD
- Deciders: Temper core maintainers
- Supersedes: ADR-XXXX (if applicable, remove if not)
- Related:
  - ADR-XXXX: Related decision
  - `.vision/FILE.md` (relevant vision constraint)
  - `crates/...` (affected code paths)

## Context

Why is this decision needed? What problem does it solve? Reference prior ADRs that created the gap this ADR fills. Be specific about what is broken or missing today.

## Decision

What was decided. For large decisions, break into sub-decisions:

### Sub-Decision 1: Title

Description with rationale. Include code examples, spec patterns, or API shapes where they clarify the design.

**Why this approach**: Explain the reasoning, not just the choice.

### Sub-Decision 2: Title

...

## Rollout Plan

(Include for changes that affect running systems or have phased delivery.)

1. **Phase 0 (Immediate)** — What ships in the first PR.
2. **Phase 1 (Follow-up)** — Next steps, integration testing.
3. **Phase N** — Production readiness.

## Readiness Gates

(Include for changes that gate a milestone like production launch.)

- Gate condition 1
- Gate condition 2

## Consequences

### Positive
- What becomes possible or easier.

### Negative
- What becomes harder, what tradeoffs are accepted.

### Risks
- What could go wrong. Mitigations if known.

### DST Compliance
(Include when changes touch simulation-visible crates: temper-runtime, temper-jit, temper-server.)

- How determinism is preserved.
- Any `// determinism-ok` annotations and why.

## Non-Goals

What is explicitly out of scope for this decision.

## Alternatives Considered

1. **Alternative name** — Description. Why rejected.
2. **Alternative name** — Description. Why rejected.

## Rollback Policy

(Include when the change is hard to reverse.)

How to undo this decision if it proves wrong.
