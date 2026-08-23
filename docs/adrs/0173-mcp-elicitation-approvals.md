# ADR-0173: In-Chat Governance Approvals via MCP Elicitation

- Status: Accepted
- Date: 2026-08-23
- Deciders: Temper core maintainers
- Related:
  - ADR-0033: Platform-assigned agent identity
  - ADR-0165: MCP trajectory tenant and bounds (stdio framing)
  - ADR-0172: Operator bootstrap `manage_policies` and self-approval block (ARN-389)
  - MCP specification 2025-06-18, "Elicitation" (client feature)
  - `crates/temper-mcp/src/elicit.rs` (this decision's implementation)
  - `crates/temper-server/src/api/decisions.rs` (approve / deny)
  - `crates/temper-cli/src/decide/mod.rs` (`temper decide` scope choices)

## Context

When Cedar denies an agent's `temper.*` call, the server creates a pending
decision and the MCP tool result carries
`{"status": "authorization_denied", "decision_id": "PD-..."}`. Resolving
that decision requires a human with `manage_policies` — via the Observe UI
or the `temper decide` terminal.

In practice the Observe UI is often not deployed next to the agent, and
`temper decide` needs a second terminal. The agent's only move is to tell
the human to go somewhere else, which breaks the loop the denial was
supposed to gate: the human is right there, in the chat, but has no
approval channel inside the harness.

MCP elicitation (spec revision 2025-06-18) is exactly that channel: the
*server* sends an `elicitation/create` request to the client, the client
harness renders it to the human, and the human's answer comes back as a
correlated JSON-RPC response. The model never sees or answers the
elicitation — that property is the whole point. A harness that supports it
declares the `elicitation` capability during `initialize`.

`temper mcp` (crates/temper-mcp) was a strictly response-only stdio loop:
read one line, dispatch, write at most one response. It could not send a
server→client request at all.

## Decision

When a `temper.*` call inside `execute` returns a structured Cedar denial
with a decision id, and the connected MCP client declared the
`elicitation` capability, the MCP process pauses the tool result and asks
the human to resolve the decision inline. Capability-gated, fail-closed to
pending, disabled with one env var.

### Sub-Decision 1: Interception point is the dispatch result

The denial is recognized where the temper HTTP response flows back through
the MCP: the sandbox dispatch callback in `run_execute`. A dispatched
result whose value carries `status == "authorization_denied"` and a
decision id is recorded (tenant, decision id, reason) — the value the
sandboxed Python code sees is never mutated. After the sandbox completes,
the recorded denials ride back with the tool result.

**Why this approach**: intercepting the final tool text instead would miss
denials the Python code transformed, and mutating mid-execution values
would change sandbox semantics. Recording at the pass-through point sees
every denial exactly once.

### Sub-Decision 2: Correlated server→client requests in the stdio loop

The loop is restructured into a reader task and a writer task joined by
channels. The reader classifies each inbound frame: a JSON-RPC *response*
(id + result/error, no method) resolves the matching entry in a pending
server→client request map; everything else queues for the sequential
dispatch loop. `ClientRequester` allocates ids from a server-side counter
and awaits the oneshot with a timeout.

Because dispatch stays strictly sequential, at most one elicitation is in
flight per session by construction — later client requests wait in the
queue. Within one tool call, only the first unique denial is elicited;
further pending decision ids are reported in the annotation so the model
retries them individually.

**Why this approach**: elicitation requires a server-initiated request
with an awaited response, which a read-dispatch-write loop cannot express.
Channels keep the existing `dispatch_json_value(&mut ctx, ...)` shape (and
its tests) intact.

### Sub-Decision 3: Capability gating and honest version negotiation

The `initialize` handler records whether `capabilities.elicitation` was
declared. Without it — or with `TEMPER_MCP_ELICIT_APPROVALS=0`, or with no
`TEMPER_API_KEY` to resolve decisions with — behavior is exactly as
before: the denial passes through untouched. Capability gating is also the
non-interactive detection: a headless client simply never declares
elicitation.

Elicitation exists from spec revision 2025-06-18, but the server always
answered `2024-11-05`. `initialize` now echoes a supported requested
revision (2025-06-18, 2025-03-26, 2024-11-05) and answers the latest
supported one otherwise, per the MCP lifecycle spec.

### Sub-Decision 4: One flat enum question, scopes mirroring `temper decide`

The elicitation uses the spec's restricted flat-object schema: a single
required string field `decision` with
`enum: [approve_narrow, approve_broad, deny, leave_pending]` and display
names, plus a message naming the denial reason, tenant, decision id, and
agent. The two approve choices map onto the real
`PolicyScopeMatrix` shapes the approve endpoint takes (verified against
`temper decide`):

- `approve_narrow` → `this_agent / this_action / this_resource / always`
- `approve_broad` → `this_agent / all_actions_on_type / any_of_type / always`

On approve, the MCP POSTs
`/api/tenants/{tenant}/decisions/{id}/approve` with `{"scope": <matrix>}`;
on deny, `/api/tenants/{tenant}/decisions/{id}/deny` — both with the MCP's
own configured credential (`TEMPER_API_KEY`, the human operator's key) as
the bearer.

### Sub-Decision 5: Fail closed; the model owns the retry

A decision is resolved only on an explicit `accept` with a recognized
choice. Decline, cancel, timeout (default 120s,
`TEMPER_MCP_ELICIT_TIMEOUT_SECS`), a malformed answer, or a closed channel
leaves the decision pending and the tool result unchanged. The MCP never
retries the denied action itself: on approval the tool result is annotated
(`"approval": "granted by human via elicitation"`, decision id, scope,
`"retry": "re-invoke the original action now"`) and the model re-invokes.
A failed resolution HTTP call is surfaced in the annotation, never
swallowed.

## Consequences

### Positive

- A Cedar denial becomes a one-question approval in the same chat, with
  no Observe UI deployment and no second terminal.
- The human channel is structural, not conventional: the harness renders
  `elicitation/create` to the human; the model cannot see or answer it.
- Clients without the capability, sessions without an operator key, and
  operators who set the kill switch get byte-for-byte today's behavior.

### Neutral / caveats

- The human identity is whatever credential the MCP process is configured
  with. The approval is recorded against the operator credential, not
  against a per-human identity.
- The self-approval block from ARN-389 still applies server-side: if the
  denied call was made under the *same* principal the MCP resolves with
  (single-credential setups where `TEMPER_API_KEY` is both the agent's and
  the operator's identity), the approve/deny returns 403 and the
  annotation surfaces that error. Distinct agent credentials are the
  supported shape.
- To make this work on the credential-bound edge, the MCP takes an optional
  `TEMPER_MCP_APPROVER_KEY`: `TEMPER_API_KEY` carries the agent's scoped
  credential (the *asker*), and `TEMPER_MCP_APPROVER_KEY` carries the
  operator/human credential used *only* to post the approve/deny (the
  *approver*). Two distinct principals satisfy the self-approval guard.
  When `TEMPER_MCP_APPROVER_KEY` is unset, resolution falls back to
  `TEMPER_API_KEY` (single-principal dev mode, subject to the block above).
  Proven live 2026-08-23: agent `claude-code` denied → human accepted inline
  → resolved as `operator` (status `approved`). The productized version of
  this two-key wiring is the MCP OAuth flow (see the linked Linear ticket).
- An elicitation blocks the session's dispatch queue for up to the
  timeout; concurrent client requests wait behind it.

### Negative

- The stdio loop now spawns two tasks and a pending-request map — more
  moving parts than the old read-dispatch-write line loop.
- A denial without a parseable `PD-` decision id cannot be elicited and
  falls back to pass-through.
