# Human-approved installed-policy replacement

## User outcome

A person operating Temper only through Foundry can review and apply an exact replacement of an installed Cedar policy. The agent proposes content; the connector's native human channel authorizes administration. Ordinary decision approvals continue to add permits and never silently remove forbids.

## Contract

Expose a separate `request_policy_replacement` MCP tool. Inputs identify a stored policy entry, its expected content hash, and proposed Cedar text; they contain no approval, credential, server override, or tenant override. Target origin and tenant come from trusted connector configuration. Evaluator labels such as `policy924` are not assumed to be durable entry identifiers.

Read the stored entry inside the connector using its separately configured approver identity. Current policy reads require `manage_policies`; never grant that permission to the requester to make this flow work. The read supplies the native human card and is not returned as an administrative policy dump. Require verified distinct requester/approver identities and native elicitation. Present the target, durable entry ID, current Cedar, proposed Cedar and hashes without hiding truncation. Bound request size; refuse oversized proposals rather than approving unseen text. Only a correlated native response accepting this specific replacement may construct consent. A generic `approve_broad`, an execute-code string, a tool argument, cancellation, disconnection or malformed response grants nothing.

After acceptance, the connector calls a supported conditional policy replacement endpoint with its separate approver identity. Server-side `manage_policies` authorization remains mandatory. It must atomically compare stored content hash and enabled state before committing. A stale match returns conflict and requires a new proposal; no automatic overwrite or implicit reapproval. Syntax validation must not activate uncommitted text. Read durable state and active policy after success; an ambiguous or failed result must not be labeled applied.

One proposal replaces one stored entry. It does not grant the requester administrative rights, disable policies, modify other entries, approve other decisions, or bypass contributor/finalizer controls. The Morrow Ink replacement removes the two attribution actions from obsolete curator-only lists; the deployed status-scoped attribution forbid remains active.

## State model

| State | Event | Next state | Mutation allowed |
|---|---|---|---|
| Unread | Valid proposal and authorized read | AwaitingHuman | None |
| AwaitingHuman | Exact native acceptance | Consented | None |
| AwaitingHuman | Other response/disconnect | Unchanged | None |
| Consented | Backend auth denied | Denied | None |
| Consented | Stored hash/state differs | Conflict | None |
| Consented | Validation fails | Invalid | None |
| Consented | Conditional durable write succeeds | Committed | Exact one entry |
| Committed | Stored and active readback agree | Verified | None |
| Committed | Verification fails | Unverified | None; report actual state |

Invariants: no mutation before native consent; no requester self-approval; no stale replacement; no activation before durable commit; unchanged unrelated entries; no success claim before readback.

## Evidence

Use the real stdio JSON-RPC approval flow and a real local Temper HTTP server, following existing setup integration tests. Cover acceptance, refusal, malformed/forged responses, disconnected clients, missing or identical credentials, invalid destinations, stale edits, invalid Cedar, persistence failure, and readback mismatch. Assert both database and active-engine behavior, including authorization before and after the approved change. Exercise the conditional store operation with deterministic conflict/failure sequences. A regression must fail against the original connector, which exposes no replacement tool.

Production completion additionally requires installing the reviewed connector in the actual Foundry session. A standalone local binary cannot receive or impersonate the session's human response and must not be used as a substitute.
