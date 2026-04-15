# ADR-0043: WASM Host-Injected Auth Headers for Internal API Calls

- Status: Accepted
- Date: 2026-04-15
- Deciders: Temper core maintainers
- Related:
  - ADR-0033: Platform-Assigned Agent Identity
  - `crates/temper-wasm/src/host_trait.rs` (ProductionWasmHost)
  - `crates/temper-authz/src/context.rs` (SecurityContext)

## Context

WASM integration modules make HTTP calls to the Temper API through the `WasmHost::http_call()` host function. Currently, each WASM guest is responsible for constructing its own auth headers (`x-temper-principal-kind`, `x-temper-principal-id`, `x-temper-agent-type`, `Authorization`). This is broken:

1. **Wrong identity**: The `monty_repl` module (which executes agent tool calls like `temper.get()`, `temper.read()`) hardcodes `x-temper-agent-type: system`. The "system" agent type is for platform-internal dispatch callbacks (Heartbeat, ProcessToolCalls), not for agent tool execution. monty_repl acts on behalf of a specific agent session and should authenticate as that agent.

2. **Duplicated and divergent header construction**: `monty_repl` has two local `runtime_headers()` functions that diverge from the canonical `wasm_helpers::runtime_headers_with_workspace()`. The duplicates omit the `Authorization: Bearer` token and hardcode the wrong agent type. Other modules (`request_approval`, `capability_installer`) also construct headers manually with missing fields.

3. **Silent auth failures**: When an internal API call returns 401/403, the host returns it as a normal response with no distinct logging. The agent sees a raw HTTP status buried in tool output. This caused a production incident where a missing auth header on `temper.read()` led to 60-minute GPT-5.4 reasoning spirals and cascading session failures.

4. **Security gap**: WASM guest code self-declares its identity. A compromised or buggy module can claim any principal kind or agent type. The host has the true identity in `WasmInvocationContext` but doesn't use it.

## Decision

### Sub-Decision 1: Host auto-injects auth headers for internal API calls

`ProductionWasmHost::http_call()` detects calls to the Temper API (URL starts with `temper_api_url` from secrets) and auto-injects auth headers from `WasmInvocationContext`:

- `x-tenant-id` from `invocation_context.tenant`
- `x-temper-principal-kind: agent`
- `x-temper-principal-id` from `invocation_context.agent_id` (falling back to `entity_id`)
- `x-temper-agent-type` derived from `invocation_context.entity_type` — "agent" for Sessions, "system" for everything else
- `x-temper-ctx-sessionid` from `invocation_context.session_id`
- `Authorization: Bearer` from `temper_api_key` secret

**Why this approach**: The host already has the true identity (from `WasmInvocationContext` populated by the dispatch pipeline). Injecting at the host level means: (a) guest modules can't get identity wrong, (b) no duplicate header construction, (c) one place to fix and audit.

### Sub-Decision 2: Guest can override for cross-tenant/admin calls

Injection only occurs when the guest has NOT set `x-temper-principal-kind`. If the guest explicitly sets principal headers (e.g., `request_approval` using admin to access GovernanceDecisions in the `temper-system` tenant), the host respects the guest's headers.

**Why this approach**: Some WASM modules make legitimate cross-tenant administrative calls. Forcing all calls through the invocation context identity would break these. The opt-out is explicit: if you set principal headers, you own the identity.

### Sub-Decision 3: Loud auth failure logging

When an internal API call returns 401 or 403, the host emits a `WARN`-level log with the URL, module name, and agent ID. This makes auth failures immediately visible in server logs.

**Why this approach**: Silent auth failures caused a multi-hour debugging session. Auth errors on internal calls are always bugs — they should be loud.

## Rollout Plan

1. **Phase 0 (This PR)** — Host injection + loud logging in `temper-wasm`. OpenPaw WASM modules updated to remove duplicate headers and rely on host injection.
2. **Phase 1 (Follow-up)** — Audit remaining `wasm_helpers::runtime_headers()` callers. As confidence grows, simplify those too.
3. **Phase 2** — Consider making host injection mandatory (reject guest-set principal headers except for a declared allowlist of cross-tenant modules).

## Consequences

### Positive
- WASM modules no longer need to construct auth headers for internal API calls — eliminates an entire class of bugs
- Auth failures are immediately visible in logs
- Identity is derived from the dispatch pipeline (authoritative), not self-declared by guest code
- Smaller, simpler WASM guest code

### Negative
- Internal vs. external call detection relies on URL prefix matching against `temper_api_url` secret — if the secret is wrong, injection won't trigger (fails open, not closed)
- Guest override mechanism means a compromised module can still spoof identity by setting principal headers

### Risks
- If `temper_api_url` is not set in secrets, no injection occurs — WASM modules with minimal headers will get 401. Mitigated by: this secret is required for all WASM modules that call the API and is already universally set.
- External APIs could theoretically share a URL prefix with the Temper API. Mitigated by: `temper_api_url` is always `http://127.0.0.1:{port}` or the deployment's internal URL.

## Non-Goals

- Credential-based identity for WASM (ADR-0033 covers this separately via `AgentCredential` entities)
- Mandatory host injection with no guest override (Phase 2, not this PR)
- Changing the Discord approval flow (uses `PawApiClient` in Rust trigger code, not WASM)

## Alternatives Considered

1. **Fix each WASM module individually** — Add correct headers to every call site. Rejected: whack-a-mole. New modules will make the same mistakes. The root cause is that auth construction is the guest's responsibility.

2. **Use `wasm_helpers` everywhere** — Delete duplicates, require all modules to import `wasm_helpers::runtime_headers()`. Rejected: still puts auth construction in guest code, still requires each module to import and call correctly, doesn't prevent future drift.

3. **Mandatory host injection with no override** — Reject any guest-set principal headers. Rejected for now: breaks `request_approval`'s cross-tenant GovernanceDecision calls. Phase 2 consideration after introducing an explicit cross-tenant call mechanism.
