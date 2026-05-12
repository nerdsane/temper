# ADR-0080: Agent-Governed Mutation Denials

- Status: Accepted
- Date: 2026-05-11
- Deciders: Temper core maintainers
- Related:
  - `crates/temper-server/src/authz/helpers.rs`
  - `crates/temper-server/src/observe/wasm.rs`
  - `crates/temper-server/src/api/decisions_access.rs`
  - `crates/temper-server/src/api/decisions.rs`

## Context

Temper already converts some Cedar denials into `PendingDecision` records so a human can approve a narrow policy and the original agent session can resume. OData actions, spec submission, and policy management follow this path. Some management mutations still returned plain `403 Forbidden` responses from local endpoint checks, notably WASM module upload and delete. Those hard denials do not create a decision, so TemperPaw cannot pause the session or surface an approval in Discord.

## Decision

Agent/session-scoped mutating requests that can be retried must convert Cedar denials into governance decisions. Temper remains the authorization authority: the endpoint still calls Cedar through `ServerState`, and the denial is recorded by Temper with the denied action, resource type, resource id, tenant, principal, and session context.

WASM module upload and delete use this governed mutation path. The resource is `WasmModule::{module_name}` and the action remains `manage_wasm`.

Requests without resumable agent context, cross-tenant observe/admin reads, and streams continue to return ordinary 403 responses. They are not converted into decisions because there is no safe session to pause and retry.

The WASM upload API continues to accept raw WASM bytes. It also accepts JSON `{ "wasm_base64": "..." }` for WASM-host clients that cannot submit binary request bodies.

Tenant-scoped decision lookup is part of the public governance API so agents do not need cross-tenant `/api/decisions` access to poll a decision they already know about. Tenant-scoped decision lists are owner-filtered for agent principals: an agent may read decisions for its own agent id or current session id, while admins and policy managers retain full list access.

## Rollout Plan

1. Add tests that show WASM upload/delete denials create pending decisions with `WasmModule::{module_name}`.
2. Move WASM upload/delete onto the governed mutation helper.
3. Add JSON base64 upload compatibility and tenant-scoped decision lookup/list access.
4. Update TemperPaw to use tenant-scoped decision routes and pin the merged Temper commit.

## Consequences

### Positive

- Agent-facing management mutations use one approval lane.
- Discord approval routing can work for delegated sessions once Temper creates a decision.
- WASM uploads from TemperPaw no longer hit a body-format dead end after approval.

### Negative

- Mutation endpoints must classify whether a denial is resumable before recording a decision.

### DST Compliance

This touches `temper-server`. The change does not introduce simulated time, random IDs, or background work beyond existing `record_authz_denial` behavior. Existing `tokio::spawn` approval callbacks are unchanged.

## Non-Goals

- Do not convert passive observe reads, cross-tenant listing, or SSE streams into governance decisions.
- Do not move Cedar authorization into TemperPaw.
- Do not create endpoint-specific approval lanes.

## Alternatives Considered

1. **Let TemperPaw synthesize decisions from 403 responses** — rejected because TemperPaw would become a second authorization layer and could not safely recreate Cedar resource scope.
2. **Grant blanket WASM management permissions to Paw agents** — rejected because it bypasses the human approval model.
3. **Expose raw binary upload support in every WASM client immediately** — rejected as unnecessary for this fix; JSON base64 compatibility preserves the raw-byte API and unblocks current clients.

## Rollback Policy

Revert the governed mutation helper use from WASM upload/delete and remove JSON base64 parsing. Tenant-scoped decision lookup can remain because it is additive.
