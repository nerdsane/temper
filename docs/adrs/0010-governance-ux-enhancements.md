# ADR-0010: Governance UX Enhancements

- Status: Accepted
- Date: 2026-02-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0008: Agent governance UX (base decisions page, SSE streaming, approve/deny)
  - ADR-0005: Agent policy and audit layer (Cedar authorization)
  - `ui/observe/app/decisions/page.tsx` (decisions page)
  - `ui/observe/lib/decision-notifier.tsx` (new notification provider)

## Context

ADR-0008 Phase 0 delivered the core governance UX: a decisions page with SSE streaming and approve/deny with scope selection. Post-deployment feedback identified four gaps:

1. **No global alerting** — Decisions are invisible unless the user is on `/decisions`. If the user is on the Dashboard or any other page when an agent triggers a Cedar denial, they have no idea.
2. **Sensitive data exposure** — `resource_attrs` in PendingDecisions can contain API keys, tokens, and secrets. These render in plaintext in the DecisionCard.
3. **Blind approval** — Users select a scope (narrow/medium/broad) and click "Allow" without seeing what Cedar policy will be generated.
4. **Flat history** — The decision history table is an unsorted flat list with no time grouping, no way to see generated Cedar policies for past approvals, and no export for compliance.

All four improvements are **frontend-only changes** — no backend modifications required.

## Decision

### Sub-Decision 1: Global DecisionNotifierProvider at Layout Level

A React context provider wraps the entire app at the layout level, inside the existing `ConnectionProvider`. It opens an SSE EventSource to `/api/decisions/stream` via `subscribeAllPendingDecisions()` and renders toast notifications for new pending decisions regardless of which page the user is on.

**Why this approach**: Follows the same pattern as `ConnectionProvider`. The layout never unmounts during SPA navigation, so the SSE connection persists across page changes. EventSource provides automatic reconnection on drop.

Toast behavior: Individual cards for 1-3 toasts, collapsing to an aggregate "N decisions need approval" for 4+. Auto-dismiss after 15 seconds. A sidebar badge shows the pending count on the "Decisions" nav item.

### Sub-Decision 2: Client-Side Secret Redaction

A pure utility function `redactSensitiveFields()` recursively walks JSON objects and replaces values for keys matching a sensitive-key set (authorization, api_key, token, secret, password, etc.) with `"[redacted]"`.

**Why this approach**: Server retains raw data for policy generation and logging. Redaction is purely a display concern — the client-side utility prevents accidental exposure in the UI without requiring backend changes. The sensitive-key list covers common patterns and can be extended.

### Sub-Decision 3: Client-Side Policy Preview

A pure function `generatePolicyPreview()` mirrors the Rust `PendingDecision::generate_policy()` logic, generating a Cedar policy string from (agentId, action, resourceType, resourceId, scope). Shown inline in the DecisionCard as a toggleable `<pre>` block that updates live when the user changes the scope selector.

**Why this approach**: Avoids a new backend endpoint. The policy generation logic is deterministic and simple enough to replicate client-side. Users can see exactly what they're approving before clicking "Allow".

### Sub-Decision 4: Audit Trail Enhancements

Decision history is grouped into "Today" / "Yesterday" / "Older" time buckets. Approved decisions have expandable rows revealing the generated Cedar policy text. A JSON export button downloads the current filtered decision set as `temper-decisions-YYYY-MM-DD.json`.

**Why this approach**: Time grouping provides natural visual hierarchy. Expandable policy rows let users audit what was approved. JSON export enables compliance workflows without requiring a dedicated reporting backend.

## Rollout Plan

1. **Phase 0 (This PR)** — All four frontend improvements: global notifications, secret redaction, policy preview, audit trail.
2. **Phase 1 (Follow-up)** — Server-side policy preview with validation (preview endpoint returns the actual Cedar policy the server would generate, ensuring client/server parity).
3. **Phase 2** — MCP elicitation integration (structured approval dialogs surfaced through MCP).
4. **Phase 3** — Desktop menubar app for governance notifications outside the browser.

## Consequences

### Positive
- Users are alerted to pending decisions from any page
- Sensitive data is no longer visible in the UI
- Users can preview policies before approving — informed consent
- Decision history is organized chronologically and exportable

### Negative
- Client-side policy preview could drift from server logic if the Rust implementation changes
- Redaction is display-only — raw data is still transmitted over the network (server-side redaction deferred to Phase 1)

### Risks
- Toast notification fatigue if an agent triggers many rapid denials — mitigated by collapse behavior (>3 toasts aggregate) and auto-dismiss
- SSE connection overhead from having two concurrent EventSource connections (one on /decisions page, one global) — mitigated by browser connection pooling and the existing reconnect-on-error pattern

## Non-Goals

- MCP elicitation (Phase 2)
- Desktop menubar app (Phase 3)
- Server-side secret redaction
- Multi-agent approval delegation
- Notification preferences / muting
