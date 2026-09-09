# Decisions & Tradeoffs

## Decision

Keep the existing host path documented as the current implementation until migration completes.

Came up because the PR marked ADR-0099 Superseded while ADR-0157 remained Proposed.

Options: retain contradictory statuses; declare the runtime migrated; distinguish accepted replacement design from current implementation.

Chose explicit design and implementation status because approval does not implement the SDK.

Where: PR #412, docs/adrs/0099-local-wasm-tdata-host-path.md and docs/adrs/0157-metadata-generated-typed-module-data-sdk.md.


## Full-panel corrections

Decision: carry complete read tokens over the ABI, reserve acknowledgement capacity before dispatch, state the existing result-omission rule consistently, and renumber this design ADR-0176.

Came up because round one identified contradictions between the exact ABI and its guarantees, and ADR-0157 collisions on current main.

Options: weaken entity-token validation and acknowledgement guarantees; leave conflicting prose; align the concrete request, capacity reservation, and result promises.

Chose to align the concrete contract because the existing intended guarantees require those fields and reservations. No runtime implementation is added. ADR-0176 was verified unused on main aa22bf13. Interrupted execution is explicitly an unknown outcome, not a failed-write acknowledgement.

Where: PR #412, docs/adrs/0176-metadata-generated-typed-module-data-sdk.md, Sub-Decisions 4 and 8; ADR-0099 supersession link.
