# ADR-0032: Agent REPL Interface for Code Mode

- Status: Proposed
- Date: 2026-03-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0006: Spec-aware agent interface for MCP
  - ADR-0016: Verification cascade hardening
  - ADR-0030: Hash-gated verification
  - `crates/temper-mcp/src/`
  - `crates/temper-verify/src/cascade.rs`
  - `crates/temper-jit/src/`

## Context

Today, agents using `temper-mcp` can only interact with a **running** Temper server: they submit Python code via the `execute` tool, which dispatches to a live server's OData API. This means every spec iteration requires:

1. Authoring an IOA spec
2. Submitting it to a server (`temper.submit_spec(...)`)
3. Waiting for the full verification cascade (L0–L3) to run server-side
4. Reading back cascade results

For iterative spec development — especially early-stage entity design — this round-trip is too slow and too opaque. Agents have no way to run a quick "does this transition make sense?" check without an active server session.

The result: agents submit underspecified or broken specs to the server, then iterate via trial and error. The server's cascade becomes a linter rather than a gate.

What is missing is a **local REPL for spec reasoning**: fast, in-process verification feedback and transition simulation that requires no server.

## Decision

Add two new tools to `temper-mcp`'s MCP server:

1. **`verify_spec`** — parse and verify an IOA spec snippet locally (L0: syntax + lint; L1: model check via Stateright). Returns structured diagnostics with per-state and per-action detail.
2. **`simulate_transition`** — given an IOA spec, a current entity status, an action name, and optional params JSON, evaluate the transition locally using `TransitionTable::evaluate`. Returns the result status, guard failures, invariant violations, and which effects would fire.

Both tools are server-independent: they use `temper-spec` for parsing, `temper-jit` for TransitionTable construction, and `temper-verify` for L1 model checking. No live Temper server is required.

### Sub-Decision 1: `verify_spec` tool

```
verify_spec(spec: str) -> VerifyResult
```

**Input**: Raw IOA TOML string (e.g. a `[[action]]` block or a full `[automaton]` spec).

**Steps**:
1. Parse via `temper_spec::automaton::parse_ioa_toml()` — returns parse errors as L0 diagnostics.
2. Build `TransitionTable` via `TransitionTable::from_ioa_source()` — returns lint errors (unreachable states, duplicate transitions, malformed guards).
3. Run `temper_verify::checker::verify()` (L1 model check via Stateright) — returns counterexamples for safety violations.

**Output** (JSON):
```json
{
  "ok": false,
  "levels": {
    "l0_parse": { "pass": true, "errors": [] },
    "l0_lint": { "pass": false, "errors": ["state 'Archived' is unreachable from initial state 'Draft'"] },
    "l1_model_check": { "pass": true, "counterexample": null, "states_explored": 12 }
  },
  "summary": "1 lint error. Fix before submitting to server."
}
```

**Why this approach**: `TransitionTable::from_ioa_source()` is already the production parse+lint path used in the spec verification hooks. Re-using it here gives agents the exact same lint feedback they would get server-side, without the round-trip. L1 (Stateright) is pure Rust with no external deps, so it runs in any Claude Code environment.

L0 SMT (Z3) is explicitly excluded — it requires an external binary that may not be present, and L1 already catches the structural safety properties agents care about during iteration.

### Sub-Decision 2: `simulate_transition` tool

```
simulate_transition(spec: str, from_status: str, action: str, params: object) -> SimResult
```

**Input**: IOA TOML string + current status + action name + optional params (JSON object).

**Steps**:
1. Parse + build `TransitionTable` (same as verify_spec steps 1–2).
2. Construct a minimal `EntityState { status: from_status, data: params }`.
3. Call `TransitionTable::evaluate(action, entity_state, params)`.
4. Return the evaluation result.

**Output** (JSON):
```json
{
  "ok": true,
  "from_status": "Draft",
  "action": "Submit",
  "to_status": "Pending",
  "effects": ["notify_reviewers"],
  "error": null,
  "guard_failures": []
}
```

On invalid transition:
```json
{
  "ok": false,
  "from_status": "Archived",
  "action": "Submit",
  "to_status": null,
  "error": "no transition from 'Archived' on 'Submit'",
  "guard_failures": []
}
```

**Why this approach**: Agents currently have to submit an entity to a server and fire an action to see if a transition is valid. `simulate_transition` short-circuits this by running the same evaluation logic locally. The `TransitionTable::evaluate()` path is identical to what the entity actor uses at runtime, so results are authoritative.

### Sub-Decision 3: Dependency additions to `temper-mcp`

Add to `crates/temper-mcp/Cargo.toml`:
```toml
temper-spec  = { workspace = true }
temper-jit   = { workspace = true }
temper-verify = { workspace = true }
```

`temper-jit` must NOT depend on `temper-verify` in production. This is not violated here: the dependency edge is `temper-mcp → temper-verify` and `temper-mcp → temper-jit`, which are separate edges. `temper-jit` itself remains verify-free.

`temper-mcp` is developer tooling (not a production binary), so pulling in `temper-verify` (which depends on `stateright`) is acceptable.

### Sub-Decision 4: Tool schema in MCP protocol

Both tools are added alongside the existing `execute` tool in `tool_definitions()` and the `tools/call` dispatcher in `protocol.rs`. They accept a JSON object argument:

```json
{ "spec": "...", "from_status": "Draft", "action": "Submit", "params": {} }
```

The `spec` field is always required. `from_status`, `action`, and `params` are required only for `simulate_transition`.

### Sub-Decision 5: Error reporting format

All errors include:
- `line` / `column` when available (from parse errors)
- `code` — a short machine-readable tag (e.g. `UNREACHABLE_STATE`, `NO_TRANSITION`, `INVARIANT_VIOLATION`)
- `message` — human-readable explanation

This structured format lets agents parse the output and surface actionable suggestions rather than opaque strings.

## Rollout Plan

1. **Phase 0**: Add `verify_spec` and `simulate_transition` tools to `temper-mcp`. No server changes. Agents can use both tools without a running server.
2. **Phase 1**: Update `temper-agent.md` skill to document the new tools. Add examples showing the iterative workflow: `verify_spec` → fix → `verify_spec` → `simulate_transition` → `execute` (submit to server).
3. **Phase 2**: Consider surfacing REPL feedback in the Observe UI (out of scope for this ADR).

## Consequences

### Positive
- Agents get instant spec feedback without a server round-trip, enabling much faster iteration.
- L1 model check catches safety violations (e.g. missing terminal states, guard contradictions) before the spec ever reaches the server's cascade.
- `simulate_transition` makes entity behavior observable and testable without persistent state.

### Negative
- L0 SMT (Z3) is not available locally — agents must use the server cascade for Z3-level algebraic verification. This is documented clearly in tool descriptions.
- Adding `temper-verify` to `temper-mcp` increases the binary size and compile time of the MCP server.

### Risks
- L1 model check can be slow for large state spaces (many states × many transitions). Mitigation: cap Stateright exploration at a configurable bound (default: 10,000 states); return a `"state_limit_reached": true` flag in results.
- Results from local verification and server cascade must remain consistent. Mitigation: both paths use the same `TransitionTable::from_ioa_source()` and `temper_verify::checker::verify()` — sharing code ensures consistency.

### DST Compliance

`temper-mcp` is not a simulation-visible crate. The new tools are synchronous and run on the async executor thread via `spawn_blocking` (for the Stateright model check, which is CPU-bound). No DST rules apply.

## Non-Goals

- Full L0–L3 cascade in the REPL (L2 DST and L3 property tests require more setup).
- A persistent REPL session with accumulated state (each tool call is stateless).
- Spec submission to the server (that remains the `execute` tool's domain).
- UI/UX for the Observe console (tracked separately).

## Alternatives Considered

1. **Extend the `execute` Python tool to call a `temper.verify_spec()` method** — would require a live server to proxy the verification call. Rejected: the whole point is to work without a server.
2. **Add a dedicated `temper-repl` binary** — a separate CLI REPL process. Rejected: agents access Temper through the MCP server; adding a second channel creates fragmentation. Keeping it as MCP tools means no new process management.
3. **Use the existing `POST /api/verify` server endpoint** — already exists. Rejected: requires a running server, defeating the goal.
