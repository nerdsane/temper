# Decisions & Tradeoffs

## Decision

Keep the existing host path documented as the current implementation until migration completes.

Came up because the PR marked ADR-0099 Superseded while ADR-0157 remained Proposed.

Options: retain contradictory statuses; declare the runtime migrated; distinguish accepted replacement design from current implementation.

Chose explicit design and implementation status because approval does not implement the SDK.

Where: PR #412, docs/adrs/0099-local-wasm-tdata-host-path.md and docs/adrs/0176-metadata-generated-typed-module-data-sdk.md (originally numbered 0157).


## Full-panel corrections

Decision: carry complete read tokens over the ABI, reserve acknowledgement capacity before dispatch, state the existing result-omission rule consistently, and renumber this design ADR-0176.

Came up because round one identified contradictions between the exact ABI and its guarantees, and ADR-0157 collisions on current main.

Options: weaken entity-token validation and acknowledgement guarantees; leave conflicting prose; align the concrete request, capacity reservation, and result promises.

Chose to align the concrete contract because the existing intended guarantees require those fields and reservations. No runtime implementation is added. ADR-0176 was verified unused on main aa22bf13. Interrupted execution is explicitly an unknown outcome, not a failed-write acknowledgement.

Where: PR #412, docs/adrs/0176-metadata-generated-typed-module-data-sdk.md, Sub-Decisions 4 and 8; ADR-0099 supersession link.

## ABI wording completion

Decision: Use the existing pre-dispatch capacity error for response-byte exhaustion and state the scalar JSON payload shape explicitly.

Came up because round two found an undefined return for failed byte reservation and a mismatch between the named tagging mode and wire examples.

Options: add another ABI mechanism; leave the cases undefined; clarify the existing error code and object shape.

Chose the existing `-3` capacity error and a named `value` field for scalar payloads because these complete the already-proposed contract without new capabilities or runtime code.

Where: PR #412, ADR-0176 Sub-Decision 4.

Scope constraint: This PR accepts a design. Review rounds may correct contradictions in that design; they do not authorize SDK implementation, migration tooling, broader architecture changes, or unrelated improvements.
