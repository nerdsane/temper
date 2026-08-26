# ADR-0188: Typed WASM Guest Terminal Failures

- Status: Proposed
- Date: 2026-08-26
- Deciders: Temper core maintainers
- Related:
  - ADR-0008: Agent Governance UX
  - ADR-0152: Fail-Closed WASM Trigger Outcomes
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0187: Versioned Application Failure Envelopes
  - `crates/temper-failure`
  - `crates/temper-wasm-sdk`
  - `crates/temper-wasm`
  - `crates/temper-server/src/state/dispatch/wasm`

## Context

ADR-0187 introduced one canonical, bounded `FailureEnvelopeV1` and the
canonical `failure_v1` callback parameter. WASM engine and host failures can
now be classified without parsing display text, but a guest-declared terminal
failure still crosses the invocation boundary as the legacy free-form
`set_error_result(&str)` shape. The kernel deliberately adapts that shape to
`permanent / LegacyFreeFormFailure / reconcile / unknown`; it cannot safely
infer provider availability, authorization, budget, integrity, or external
commit facts from diagnostic prose.

The invocation result boundary is also too permissive for a typed contract.
`host_set_result` currently copies any in-memory payload that fits guest linear
memory, multiple writes overwrite one another, and the legacy positive return
pointer path allocates from its guest-provided length after checking only the
linear-memory range. Generic JSON parsing accepts omitted and unknown fields.
A typed declaration would therefore be unsafe unless the transport and parser
fail closed before application routing.

The new guest surface must extend ADR-0187 rather than introduce a second
application failure envelope. Guests may state bounded application facts. Only
the kernel may state causal identity and provenance, construct the final
envelope, select a verified category route, or decide what can be exported.

## Decision

Add one versioned `GuestFailureDeclarationV1` contract to `temper-failure`,
re-export it from `temper-wasm-sdk`, validate terminal results centrally in
`temper-wasm`, and adapt accepted declarations to `FailureEnvelopeV1` in the
server WASM dispatcher. The existing verified `failure_v1` callback ABI and
category route syntax remain unchanged.

### Sub-Decision 1: Pin Three Exclusive Terminal Result Shapes

The v1 invocation result is exactly one of the following compact JSON object
shapes. JSON object member order is not semantically significant to raw guests;
the SDK emits the shown order and golden tests pin its exact bytes.

Success preserves the existing SDK wire form:

```json
{"action":"ChargeSucceeded","params":{"provider_id":"p-1"},"success":true}
```

Its allowed top-level fields are exactly `action`, `params`, and `success`.
`action` is a string, `params` is any JSON value within the complete result
budget, and `success` is the literal `true`. An empty `action` preserves the
existing side-effect-only convention: the invocation succeeds without
dispatching a callback.

Legacy failure preserves the existing `set_error_result(&str)` wire form:

```json
{"action":"callback","params":{"error":"provider rejected request"},"success":false,"error":"provider rejected request"}
```

Its allowed top-level fields are exactly `action`, `params`, `success`, and
`error`. `success` is the literal `false`; `action`, `params`, and `error` are
required for byte-compatible SDK output. The kernel continues to treat the
top-level `error` as untrusted diagnostic text and never parses it for control
flow. Legacy failure routing remains the explicit behavior defined by
ADR-0187.

Typed failure has no guest-selected callback action or callback parameters:

```json
{
  "success": false,
  "typed_failure": {
    "version": 1,
    "category": "transient",
    "code": "ProviderUnavailable",
    "retryability": "with_backoff",
    "outcome": "not_applied",
    "diagnostic": "provider did not accept the request",
    "details": {
      "status": {"kind":"unsigned","value":503}
    }
  }
}
```

Its allowed top-level fields are exactly `success` and `typed_failure`, and
`success` must be the literal `false`. The declaration contains exactly:

- `version`, which must be the integer `1`;
- one closed ADR-0187 `FailureCategory` value;
- one bounded ADR-0187 `StableFailureCode`;
- one closed ADR-0187 `FailureRetryability` value;
- one closed ADR-0187 `FailureOutcome` value;
- optional `diagnostic`, bounded by `MAX_DIAGNOSTIC_BYTES`; and
- `details`, defaulting to an empty `BoundedFailureDetails` map and using its
  existing tagged scalar encoding and budgets.

Unknown fields are rejected at both levels. A typed result containing `action`,
`params`, `error`, an envelope omission flag, `operation`, `provenance`, or any
other field is invalid. Success, legacy failure, and typed failure fields cannot
be mixed. Unknown enum values, future versions, invalid stable codes, invalid
category/retryability/outcome semantics, and invalid diagnostic or detail
bounds are invalid. The canonical ADR-0187 rule remains authoritative:
`outcome = unknown` if and only if `retryability = reconcile`, and an
`ambiguous` category must have an unknown outcome.

**Why this approach**: Preserving the two SDK-produced legacy shapes avoids a
gratuitous compatibility break, while the exclusive `typed_failure` member
makes guest/kernel ownership auditable. A permissive generic JSON intermediate
would make contradictory raw-guest payloads representable and would weaken the
same contract for SDK and non-SDK guests.

### Sub-Decision 2: Bound Results Before Allocation

Define `MAX_WASM_RESULT_BYTES_V1 = 1_048_576` bytes for the complete serialized
terminal result, regardless of shape or transport. The budget applies to UTF-8
bytes, not characters.

For `host_set_result(ptr, len)`, the host validates the signed length and the
1 MiB budget before copying from guest memory. For the legacy positive return
pointer path, the host reads the fixed four-byte little-endian length prefix,
checks the 1 MiB budget and the guest-memory range, and only then allocates the
result buffer. Invalid pointers, negative lengths, invalid UTF-8, and oversized
payloads are terminal validation failures; they are not interpreted as an
absent result.

The 1 MiB budget leaves headroom for existing callback parameter payloads while
placing an explicit host-allocation ceiling far below the default 64 MiB guest
linear-memory budget. The limit is protocol-owned and not configurable per
guest or integration.

**Why this approach**: A result budget derived from current guest memory is not
a protocol bound and changes when memory configuration changes. A fixed v1
budget is reviewable, testable on both transports, and checked before any
guest-sized host allocation.

### Sub-Decision 3: Require Exactly One Result Source

An invocation must use exactly one of these transport patterns:

1. Call `host_set_result` exactly once and return `0` from `run`.
2. Never call `host_set_result` and return a positive legacy result pointer of
   at least four, whose preceding four bytes contain the bounded payload length.

Zero result writes with a zero or negative return value are invalid. A second
`host_set_result` call makes the invocation invalid even when both writes are
identical; the first result is never silently replaced. One or more host writes
combined with a positive return pointer are invalid because the guest selected
multiple result sources. The host records only `none`, `one`, or `multiple`, so
an adversarial guest cannot overflow a write counter. Validation is independent
of Wasmtime scheduling and deterministic.

**Why this approach**: Last-write-wins makes malformed guests depend on call
order and hides conflicting declarations. Explicit source cardinality gives
the SDK path and legacy pointer path equal, deterministic treatment.

### Sub-Decision 4: Invalid Guest Results Become One Kernel Failure

Every boundary or semantic failure after guest execution begins maps from a
closed `InvalidGuestResultKind` variant to this kernel-owned envelope:

```text
category: ambiguous
code: InvalidGuestFailureResult
retryability: reconcile
outcome: unknown
provenance.source: wasm
provenance.component: wasm-result-validator
provenance.source_code: <closed kernel validation code>
```

This includes absent results, multiple writes or sources, invalid pointer or
length, oversized bytes, invalid UTF-8 or JSON, unknown or contradictory fields,
future declaration versions, invalid enums, invalid stable codes, and invalid
diagnostic/detail bounds. The validation code is selected from a closed kernel
enum; diagnostic text never selects a category, route, retryability, outcome,
redaction rule, or authorization behavior.

Because validation happens after `run` was entered, the kernel cannot prove
that external effects did not occur. It therefore always uses the unknown
outcome and reconciliation guidance. The server derives the same deterministic
causal operation identity used by other WASM failures and exclusively supplies
the complete provenance. If the application has no verified `ambiguous` route,
existing fail-closed undeclared-category behavior applies.

**Why this approach**: Treating malformed output as an ordinary guest trap or a
legacy failure would lose the ABI-specific stable code. Claiming `not_applied`
would be unsafe after arbitrary guest code and host calls may have run.

### Sub-Decision 5: Add A Typed SDK Authoring Path Without Replacing Legacy

`temper-wasm-sdk` re-exports the declaration and closed ADR-0187 types, exposes
`set_typed_failure_result(&GuestFailureDeclarationV1)`, and defines
`TypedModuleResult = Result<Value, GuestFailureDeclarationV1>`. A dedicated
`temper_module!` arm accepts `-> TypedModuleResult`; successful values use the
same success encoder and failures use the typed encoder. The existing
`-> Result<Value>` arm and `set_error_result(&str)` remain unchanged.

Declaration construction is fallible and validates the same shared contract
used by raw-result deserialization. Convenience methods may attach bounded
diagnostics and scalar details, but cannot truncate, add causal/provenance
fields, or choose callback actions.

**Why this approach**: Module authors can return `Err(declaration)` through the
normal macro without constructing JSON. Keeping the legacy macro arm explicit
prevents arbitrary `String` errors from being guessed into categories.

### Sub-Decision 6: Keep Callback ABI And Observation Redacted

The server converts an accepted declaration into `FailureEnvelopeV1` by copying
only category, code, retryability, outcome, bounded diagnostic, and bounded
details. It derives the causal operation and uses:

```text
provenance.source = wasm
provenance.component = wasm-guest
provenance.source_code = GuestDeclaredFailure
```

The resulting category selects the already verified failure route. The callback
remains exactly `{"failure": <FailureEnvelopeV1>}` with the existing
`failure_v1` CSDL type. No second envelope, callback field, or IOA route syntax
is introduced.

Typed-failure observation never includes the guest diagnostic. Guest details
are also omitted by default even though they are bounded scalar data. A future
kernel- or schema-owned allowlist may copy specifically named details into the
observation projection; guest-selected keys never authorize export. Callback
delivery still receives the bounded canonical envelope because application
state transitions are the governed contract consumer, not telemetry export.

**Why this approach**: Bounds protect transport and storage; they do not prove
that provider text or detail values are safe to export. Redacting the entire
guest-owned detail map is the only fail-safe default without a trusted schema.

## Rollout Plan

1. Add this ADR and the shared guest declaration contract.
2. Add SDK encoding and the typed `temper_module!` path while preserving legacy
   bytes.
3. Replace permissive result parsing with strict shape validation and enforce
   the result-size and cardinality contracts on both transports.
4. Adapt accepted declarations and invalid results in the server through the
   existing verified category routes and callback ABI.
5. Exercise the SDK guest and adversarial raw guests locally, then deploy and
   verify typed routing, redaction, and stable causal identity in Datadog.

## Readiness Gates

- Golden tests pin all three SDK/wire encodings.
- Exhaustive tests cover every category and every semantically valid
  category/retryability/outcome combination.
- Both result transports reject bytes above the exact 1 MiB boundary before
  allocation and accept bytes at the boundary when the JSON is valid.
- Zero writes, multiple writes, dual transports, invalid UTF-8/JSON, unknown
  fields/enums, future versions, oversized fields, and causal/provenance
  injection map to the pinned invalid-result envelope.
- Typed and legacy end-to-end dispatch tests prove canonical callback routing,
  undeclared-category failure, deterministic causal identity, and legacy
  compatibility.
- Observation tests prove guest diagnostics and details do not appear in spans,
  metrics, wide events, or typed-failure observe events by default.
- Mandatory code-quality and DST reviews pass with no unresolved findings.

## Consequences

### Positive

- WASM guests can report application facts without forging kernel facts.
- Raw and SDK guests receive identical bounded, fail-closed validation.
- Existing category routes and the canonical `failure_v1` callback remain the
  only application recovery mechanism.
- Host allocation and result-write behavior become explicit and deterministic.

### Negative

- Raw guests that relied on unknown top-level fields, missing required fields,
  multiple result writes, or implicit parser defaults will now fail closed.
- A 1 MiB result ceiling may require genuinely large successful outputs to move
  to existing stream/blob mechanisms.
- Guest details cannot appear in telemetry until a trusted allowlist is defined.

### Risks

- A strict parser could accidentally reject the existing SDK shapes. Exact
  golden tests and compiled legacy fixtures mitigate this.
- A declaration could carry sensitive diagnostic/detail content into an
  application callback. Existing Cedar-governed action dispatch and bounded
  values limit the surface; observation remains redacted by default.
- Incorrect semantic validation could diverge from the canonical envelope.
  Both types share the same validation function and exhaustive combination
  tests.

### DST Compliance

- Validation is pure and uses closed enums and `BTreeMap`-backed details.
- Result cardinality is invocation-local and saturates at `multiple`; no wall
  clock, randomness, process environment, filesystem, network, or new task is
  introduced.
- Causal identities use the existing deterministic integration operation
  derivation and `sim_uuid()` fallback.
- Runtime and simulation route the same canonical envelope through the same JIT
  metadata.

## Non-Goals

- Letting a guest set operation identity, attempt, parent identity, provenance,
  callback action, or failure route.
- Defining another application failure envelope or changing `failure_v1`.
- Parsing diagnostics, provider bodies, or legacy error strings for control
  flow.
- Automatically retrying any failure, especially unknown-outcome operations.
- Exporting guest diagnostics or guest-selected detail keys by default.
- Implementing TemperPaw guest adoption in the Temper kernel.

## Alternatives Considered

1. **Let guests construct `FailureEnvelopeV1`** — Rejected because operation and
   provenance are kernel attestations, not application claims.
2. **Add a new typed callback or route syntax** — Rejected because ADR-0187
   already defines the canonical envelope and verified category routing.
3. **Tag every result with a new top-level string discriminator** — Rejected
   because it would break the existing byte-stable success and legacy SDK
   outputs. The literal `success` value plus the exclusive `error` or
   `typed_failure` member provides strict discrimination.
4. **Use last-write-wins for `host_set_result`** — Rejected because it silently
   accepts contradictory terminal claims and makes malformed output order
   significant.
5. **Use the guest memory limit as the result budget** — Rejected because it is
   too large, configurable, and checked too late to be a stable protocol
   allocation bound.
6. **Export bounded guest details automatically** — Rejected because a byte
   bound is not a confidentiality classification.

## Rollback Policy

Before typed guests ship, the declaration, SDK arm, validator, and server
adapter can be reverted together without changing IOA specs or callback types.
After adoption, first redeploy guests to the existing legacy
`set_error_result` path, then revert the typed declaration support. The 1 MiB
allocation bound and explicit result cardinality should remain as independent
boundary hardening unless a separately accepted ADR replaces their constants or
transport semantics.
