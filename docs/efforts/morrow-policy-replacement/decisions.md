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
