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

## Exact amendments to large existing entries (2026-09-21)

A deployed entry can exceed the complete-text proposal budget while the required
change is only one clause. Raising the approval form limit or hiding unchanged
text would not provide a useful, complete review.

`request_policy_amendment` accepts a durable policy ID, the SHA-256 of the complete
current document, and 1–16 exact `old`/`new` text edits within 16 KiB of serialized
edits. Each old string must occur exactly once in the original document. Edits
must not overlap; replacements are applied simultaneously, preserving every
byte outside the reviewed ranges. The approval shows each complete changed
range and both whole-document hashes. Text edits are not claimed to be a Cedar
AST transformation: the existing server parses and validates the complete result.

The Foundry relay stores this compact proposal, reconstructs it from the current
hash-bound document when the human answers, and calls the existing privileged
replacement endpoint. Its verified receipt binds the entry, tenant, original
hash, result hash and SHA-256 of the edits serialized as a JSON Value. The native
non-relay flow retains separate verified requester/approver identities and
correlated human elicitation. Neither flow accepts agent-supplied consent.

The privileged replacement endpoint accepts up to 2 MiB of complete Cedar to
accommodate existing bundles. It retains prospective-policy validation, storage
compare-and-set, durable readback, and activation only after verification.
Ordinary full-text MCP proposals retain their original size bound. Decline,
cancel, stale data, ambiguous edits and malformed receipts never authorize a
write. This extends the existing operation; it adds no policy grant or bypass.
