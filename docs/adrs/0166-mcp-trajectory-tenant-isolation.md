# ADR-0166: MCP trajectory tenant isolation and bounded capture (ARN-222)

- Status: Accepted
- Date: 2026-07-12
- Related: ARN-222, `crates/temper-mcp/src/runtime.rs`

## Decision

1. Trajectory uploads always use `identity_tenant` (never majority-vote of seen tenants).
2. Cross-tenant call metadata is not accumulated into session tenant counters.
3. When execute code references any tenant other than `identity_tenant`, retain a
   redaction marker instead of raw code/results/action args under the identity
   trajectory (no durable foreign-tenant content disclosure).
4. Code/results are char-safe truncated before retention; stdio lines are
   byte-budgeted and oversized frames receive a JSON-RPC error response.
