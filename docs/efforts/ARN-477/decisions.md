# Decisions & Tradeoffs

## Decision

Keep the existing host path documented as the current implementation until migration completes.

Came up because the PR marked ADR-0099 Superseded while ADR-0157 remained Proposed.

Options: retain contradictory statuses; declare the runtime migrated; distinguish accepted replacement design from current implementation.

Chose explicit design and implementation status because approval does not implement the SDK.

Where: PR #412, docs/adrs/0099-local-wasm-tdata-host-path.md and docs/adrs/0157-metadata-generated-typed-module-data-sdk.md.
