# ADR-0163: MCP trajectory tenant isolation and bounded capture (ARN-222)

- Status: Accepted
- Date: 2026-07-12
- Related: ARN-222, `crates/temper-mcp/src/runtime.rs`

## Decision

1. Trajectory uploads always use `identity_tenant` (never majority-vote of seen tenants).
2. Cross-tenant call metadata is not accumulated into session tenant counters.
3. Code/results are char-safe truncated before retention; stdio lines are byte-budgeted.
