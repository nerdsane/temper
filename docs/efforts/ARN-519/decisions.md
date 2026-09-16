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

## D2: The TData host keeps the caller's context

**Decision:** Leave `LocalTDataWasmHost` on the trigger path bound to the
triggering caller's security context.

**Came up because:** The trigger path hands the caller's context to two
consumers, and it was tempting to rebind both for symmetry.

**Options:** Rebind both; rebind only the internal HTTP capability.

**Chose the HTTP capability only because** that is exactly what D7 did on the
endpoint path, and D7 said why: a guest that expects the kernel to scope its
TData reads must keep inheriting its caller's reach. The failing case
(object-cache PUT) travels the HTTP capability, so this is also the smallest
change that fixes it.

**Where.** `crates/temper-server/src/state/dispatch/wasm.rs`, trigger dispatch;
the TData host construction a few lines below the change is untouched.

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
