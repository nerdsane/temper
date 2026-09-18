# ADR-0178: Exact human-approved policy replacement

- Status: Proposed
- Date: 2026-09-18
- Related: ADR-0177; `docs/efforts/morrow-policy-replacement/spec.md`

## Context

An ordinary grant cannot override a Cedar forbid. Native MCP offers human decision approval but cannot currently replace an obsolete installed policy. Existing policy PATCH also lacks an atomic check against the version the human reviewed.

## Decision

Expose a separate `request_policy_replacement` proposal tool. Its only inputs are a durable policy ID, expected SHA-256 and proposed Cedar text. Configured origin and tenant cannot be overridden. The connector reads the current entry, discloses the complete old and new text to its native human client, and requires exact correlated acceptance. Cancellation, unrelated approval and tool arguments never supply consent. The separately configured approver identity is verified distinct from the requester and used only after acceptance for mutation; normal backend policy authorization still applies.

Add an authorized per-entry conditional replacement operation. Parse Cedar before persistence, atomically update only an enabled row with the expected hash, then reload and verify durable and active state. Missing/disabled/stale entries conflict. No retry rewrites a different version. Storage failures never activate uncommitted proposals. Reload/readback failures report committed-but-unverified state, never success.

## Rollout and verification

Test protocol rejection paths, competing proposals and durable failures, then exercise native elicitation against a real isolated server. Deploy backend and native connector; verify the advertised tool before requesting live human consent. The existing execute boundary remains intact.

## Consequences

A human can replace a specific stale rule without granting the agent policy administration. Large entries exceeding the bounded card budget require another human administration workflow. A durable commit followed by an activation failure requires explicit recovery and remains an error.

## DST compliance

The server operation uses the PolicyStore boundary with injected fault schedules; it introduces no clock, randomness or filesystem access. Real database tests additionally verify the conditional SQL.

## Non-goals

Deleting policies, disabling policies, changing finalizer roles, auto-accepting native requests, and giving execute an approver credential.

## Alternatives

A broad decision grant cannot defeat a forbid. Direct privileged agent PATCH loses the human boundary. Read-then-unconditional-write loses concurrent updates. Replacing the full policy set unnecessarily changes unrelated entries.
