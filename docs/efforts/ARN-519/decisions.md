# Decisions and tradeoffs

## D1: Carry the module name on the capability; do not rebind the principal

**Decision:** The internal HTTP capability keeps the caller's identity and adds
`context.module`; a triggered integration is not given its module as
principal.

**Came up because:** The first two cuts of this effort mirrored ARN-499 D7 on
the trigger path — the integration acts as `Agent::"<module>"`. Review round 2
(codex, act-on) showed what that costs: TemperPaw's os-apps gate twelve
policies on the *triggering* principal's `agent_type` (`system`,
`file-service`), and their triggered modules make loopback GET/POST/PATCH/PUT
calls under that identity. Rebinding the principal turns those into 403s
across another repository.

**Options:** Module as principal on the trigger path plus module permits in
every TemperPaw app; module name as a context attribute with the principal
untouched; a fallback to the module only when the caller is anonymous.

**Chose the context attribute because** it is additive — no existing policy
changes meaning, no other repository has to move — and Genesis's permit was
already written to accept `context.module` for triggered integrations. The
fallback was rejected because a policy would then answer differently for the
same module depending on who triggered it.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`,
`internal_http_capability_issuer`; callers in the same file and
`api/repl.rs`. Review records rounds 1–2 on PR #474.

## D2: No security context still means no capability

**Decision:** The issuer's `None` arm is unchanged: a dispatch whose caller has
no security context gets no internal capability.

**Came up because:** An earlier cut delegated `SecurityContext::anonymous()`
in that arm, believing a public push reached it. Review round 4 (fable)
checked the path: the edge already admits a public push as the anonymous
principal (`bearer_auth`), the router copies it into the trigger context, so
the push takes the `Some` arm — the original `principal_id="anonymous"`
denial was itself proof a capability existed. The callers that really reach
`None` are sentinel and compensation dispatches (`AgentContext::default()`),
whose integrations' internal calls are refused at the edge today; delegating
anonymous there would have let a module-scoped permit admit them.

**Options:** Delegate anonymous in the `None` arm; leave the arm alone.

**Chose leaving it because** the accepted defect never needed it, and it
widened what sentinel- and compensation-driven integrations can reach.

**Where.** `internal_http_capability_issuer`; round-4 review record.

## D3: Proven live, plus one unit test on the capability

**Decision:** Acceptance evidence is the live Genesis push that failed before
and passes after, an independent verifier's rerun, and a unit test that mints
a capability and resolves it.

**Came up because:** Trigger dispatch has no test seam short of standing up
server state, but the capability round-trip does — the credential store can
resolve what the issuer minted.

**Options:** Live proof only; a dispatch fixture; the capability round-trip.

**Chose the round-trip because** it checks the contract this effort adds
(principal unchanged, `context.module` set for this hop or cleared, no
capability without a context) without faking the policy or the modules the
live push exercises.

**Where.** `state/dispatch/wasm/wasm_test.rs`; proof record on the PR.

## D4: Correlation id from `sim_uuid()`

**Decision:** `wasm_module_security_context` takes its correlation id from
`temper_runtime::scheduler::sim_uuid()` instead of `uuid::Uuid::now_v7()`.

**Came up because:** Review round 1 (fable) noted the wall-clock id sat under
a determinism suppression on a simulator-visible path.

**Options:** Keep the suppression; use the simulator-aware source the rest of
the server uses.

**Chose `sim_uuid()` because** it is what every other id on this path uses,
and it deletes a suppression instead of widening one.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`,
`wasm_module_security_context`.
