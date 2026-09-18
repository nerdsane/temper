# Decisions and tradeoffs

## Keep policy replacement separate from permit approval

**Decision:** Use a distinct native human-consent operation for replacement.

**Came up because:** Morrow Ink's narrow and broad approvals repeatedly added permits while installed forbids remained effective.

**Options:** Treat an ordinary permit approval as permission to delete a forbid; let agents invoke the administrative HTTP setter; or present an explicit replacement through the connector's existing human channel.

**Chose explicit replacement because:** It preserves the meaning of prior approvals and prevents an agent from treating a denial as authority to rewrite the denying policy. It requires a connector change and native human interaction.

**Where:** `crates/temper-mcp/src/setup.rs`, `setup_consent.rs`, `elicit.rs`; this effort's specification.

## Require a conditional backend write

**Decision:** Compare expected policy content and enabled state atomically with the durable replacement.

**Came up because:** The current administrative PATCH updates text without a compare-and-swap precondition and reloads prospective policy before its storage write. A human can take time to answer while another operator edits the same entry.

**Options:** Re-read just before the current PATCH; replace all tenant policies; or add a conditional per-entry operation.

**Chose conditional replacement because:** It closes the time-of-check/time-of-use window and preserves unrelated policy entries. It requires changes at the server/store boundary rather than a connector-only wrapper.

**Where:** `crates/temper-server/src/api/policies.rs::handle_patch_policy`, `crates/temper-server/src/storage/mod.rs::PolicyStore`.

## Preserve the configured native gateway prefix

**Decision:** Preserve the trusted connector base URL path when appending policy API routes.

**Came up because:** This Foundry session configures Temper under an authenticated `/v1/agent/native/.../temper_platform` prefix rather than a bare server origin.

**Options:** Require a bare origin and fail this supported route; bypass the gateway with a different URL; or preserve the configured prefix.

**Chose the configured prefix because:** It retains Foundry authentication and routing. Tool arguments cannot override it, redirects remain disabled, and embedded credentials/query/fragment remain rejected.

**Where:** `crates/temper-mcp/src/policy_replacement.rs::PolicyApi::new`.

## Keep policy reads inside the private approver boundary

**Decision:** Use the connector's private approver for the exact-entry pre-consent read and readback; use it for mutation only after native human consent.

**Came up because:** Existing policy-read endpoints require `manage_policies`, as confirmed in the implementation and Greptile's contract review. A non-administrative requester cannot perform the planned read.

**Options:** Grant the requester administrative access; introduce new read roles and migrate deployed policy; or let the trusted connector read the specific entry with the configured approver.

**Chose the private read because:** It uses the existing backend authorization, keeps credentials out of execute, and provides the human the exact policy without granting the requester administration. The human must still accept a correlated replacement before any write.

**Where:** `crates/temper-mcp/src/policy_replacement.rs`, real-server test, and spec contract.

## Match Foundry's server-held approval credentials

**Decision:** Add an explicitly configured host-relay mode and its corresponding Foundry proposal/answer integration.

**Came up because:** The actual session uses Foundry's native proxy, which strips operator credentials from the sandbox and allows writes only from a signed-in human answer. Deploying a direct-server connector alone cannot satisfy this task.

**Options:** Expose the approver credential to the sandbox; bypass Foundry and connect directly; or retain the host boundary and add a proposal-only relay.

**Chose the host relay because:** It preserves the deployed trust boundary. The connector can create/read the human request; the signed-in host answer route owns the conditional write. This requires a matching Foundry change and local host-flow evidence before deployment.

**Where:** `crates/temper-mcp/src/policy_replacement.rs`; Foundry `factory/native.rs`, human answer handling and native launcher.

## Serialize policy API commit and activation

**Decision:** Reuse the existing approval mutex across conditional replacement and the policy mutation handlers.

**Came up because:** Atomic row replacement protects durable content, but concurrent API writers can otherwise activate snapshots in the opposite order from their commits.

**Options:** Add another independent lock; rely on row compare-and-swap alone; or share the existing policy approval lock.

**Chose the shared lock because:** It serializes the API read/commit/reload sequence with approval writes without a new lock hierarchy. It is process-local; it does not introduce cross-instance cache synchronization or change existing installer behavior.

**Where:** `crates/temper-server/src/api/policies.rs`, `api/policies/replacement.rs`, and `state/mod.rs`.
