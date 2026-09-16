# Decisions and tradeoffs

## D1: A triggered integration acts as its module, exactly as an endpoint guest does

**Decision:** On the trigger path, feed `internal_http_capability_issuer` the
module's own security context instead of the triggering caller's.

**Came up because:** The genesis #50 round-trip failed at the push with
`internal blob object access denied … principal_id="anonymous"`. The ingest
integration was writing the object cache as its caller — and a push's caller
is anonymous by design. ARN-499's D7 fixed the same problem for HttpEndpoint
guests and stopped there.

**Options:** Widen Genesis's object-cache permit back to an unconstrained
principal; special-case the blob gate to trust triggered integrations; give
the trigger path the identity binding the endpoint path already has.

**Chose the binding because** the first is the security hole two reviewers
just had closed, the second encodes an exception where a rule belongs, and the
third makes the two invocation paths agree on what a module is. It is the
change D7 should have made in both places.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`, the trigger
dispatch host construction.

## D2: The TData host is rebound too — one identity per module, whatever the transport

**Decision:** Bind the trigger path's `LocalTDataWasmHost` to the module's
security context as well, not only the internal HTTP capability.

**Came up because:** The first cut rebound only the capability issuer and
said D7 had done the same. Review round 1 (fable) checked: the endpoint path
constructs its `LocalTDataWasmHost` with the module identity (wasm.rs ~L180),
so the first cut mirrored D7 by half — and the half left a triggered
integration split by transport: in-process GET/POST `/tdata` as the caller,
everything else as the module, so one policy answered differently by HTTP
method.

**Options:** Keep the split and document it; rebind the TData host too.

**Chose the rebind because** D7's rule is "a module is one principal", the
endpoint path already applies it to both hosts, and a split identity is the
kind of exception a reader cannot predict from the policy.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`, the
`LocalTDataWasmHost::new` call in trigger dispatch. Round-1 review record.

## D3: Proven live, not by a unit test

**Decision:** The acceptance evidence is the live Genesis push that failed
before and passes after, plus an independent verifier rerun; no unit test is
added.

**Came up because:** The changed line sits inside trigger dispatch, which has
no test seam short of standing up server state; D7 was accepted on the same
basis.

**Options:** Build a dispatch-state test fixture; prove live and record it.

**Chose live proof because** a fixture is machinery this one-line change does
not justify, and the live push exercises the real policy, real modules, and
the real gate — which a fixture would have to fake.

**Where.** Proof record on the PR.

## D4: Correlation id from `sim_uuid()`

**Decision:** `wasm_module_security_context` takes its correlation id from
`crate::sim_uuid()` instead of `uuid::Uuid::now_v7()`.

**Came up because:** Review round 1 (fable) noted that trigger dispatch is a
simulator-visible path, and the wall-clock id, harmless on the endpoint path,
now ran on every triggered invocation under a determinism suppression written
for the endpoint path.

**Options:** Keep the suppression; use the simulator-aware source the rest of
the server uses.

**Chose `sim_uuid()` because** it is what every other id on this path uses,
and it deletes a suppression instead of widening one.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`,
`wasm_module_security_context`.
