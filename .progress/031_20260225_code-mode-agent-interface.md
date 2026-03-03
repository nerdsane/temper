# Code Mode for Temper: Spec-Aware Agent Interface

## Status: COMPLETE

## Phases

### Phase 1: Search helpers (runtime.rs)
- [ ] Inject spec as Dataclass (type_id: 10) in run_search()
- [ ] Add dispatch_spec_method() for tenants/entities/describe/actions/actions_from/raw
- [ ] Change run_search() from single-shot to loop

### Phase 2: Dynamic tool descriptions (spec_loader.rs + protocol.rs)
- [ ] Add generate_loaded_summary() to spec_loader.rs
- [ ] Make tool_definitions() take &RuntimeContext
- [ ] Update search/execute descriptions with spec-aware text

### Phase 3: Developer methods (tools.rs)
- [ ] show_spec — read from self.spec
- [ ] submit_specs — POST /api/specs/load-inline
- [ ] set_policy — PUT /api/tenants/{t}/policies
- [ ] get_policies — GET /api/tenants/{t}/policies

### Phase 4: Governance methods (tools.rs)
- [ ] get_decisions — GET with optional status filter
- [ ] approve_decision — POST with scope
- [ ] deny_decision — POST
- [ ] poll_decision — loop with 1s sleep, 30s timeout

### Phase 5: Agent identity (tools.rs)
- [ ] Add X-Temper-Principal-Kind: agent header
- [ ] Add X-Temper-Principal-Id: mcp-agent header

### Phase 6: Structured errors (sandbox.rs)
- [ ] Detect 403 + AuthorizationDenied in format_http_error()
- [ ] Parse body for decision ID
- [ ] Format rich denial message with guidance

### Phase 7: Tests + verification
- [ ] Update existing search test for new spec API
- [ ] cargo build -p temper-mcp
- [ ] cargo test -p temper-mcp
