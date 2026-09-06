# Decisions and tradeoffs

## D1: Enforce contracts in the kernel

**Decision:** Add opt-in strict action parameters and pre-state constraints to IOA and the production actor boundary.

**Came up because:** Actual simulator and HTTP tests showed that an observation could overwrite an undeclared desired field and generic writes could bypass named actions.

**Options:** Depend on Cedar action names alone. add DSF-specific HTTP checks. enforce the declared contract in shared kernel execution.

**Chose shared kernel execution because:** It protects both HTTP and internal dispatch and lets the real simulator prove the behavior. The opt-in flag preserves existing applications until their specifications adopt the contract.

**Where:** crates/temper-jit/src/table/action_contract.rs. crates/temper-server/tests/strict_action_contract.rs. crates/temper-server/tests/strict_generic_writes.rs.

## D2: Reject numeric strings

**Decision:** Numeric constraints accept JSON integers only.

**Came up because:** A factory regression showed a numeric string could pass the new timestamp comparison while the existing numeric effect skipped it.

**Options:** Coerce action payloads globally. accept strings only in comparisons. use the same integer representation as effects.

**Chose integer representation because:** It removes the guard/effect disagreement without silently changing payload values or unrelated applications.

**Where:** crates/temper-jit/src/table/action_contract.rs.
