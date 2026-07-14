# ADR-0171: Reject Undeliverable IOA Webhook Integrations

- Status: Proposed
- Date: 2026-07-13
- Deciders: Temper core maintainers
- Supersedes: ADR-0046 Sub-Decision 2's webhook acceptance and known-gap
  expansion only
- Related:
  - ADR-0002: WASM Sandboxed Integration Runtime for Agent-Generated API Calls
  - ADR-0007: Governed External API Calls Through the MCP REPL
  - ADR-0046: Unified Action Triggers
  - ADR-0152: Integration Failure Is Never Silent
  - `crates/temper-spec/src/automaton/parser.rs`
  - `crates/temper-server/src/state/dispatch/mod.rs`

## Context

ADR-0046 made `[[action.triggers]]` the canonical declaration surface for
entity, WASM, adapter, and webhook work. The parser currently accepts
`kind = "webhook"`, adds a custom transition effect, and synthesizes an
`Integration` with `integration_type = "webhook"`. Production post-dispatch
orchestration only executes integrations whose type is `wasm` or `adapter`.
A valid, verified webhook trigger therefore commits its source transition and
is then silently ignored.

Adding a direct HTTP branch would make the declaration execute, but would not
make it correct. External delivery begins after the source event commits.
Background WASM dispatch is an unjournaled `tokio::spawn`; replay regenerates
custom effects and discards them; and WASM invocation artifacts are recorded
only after execution starts. A process failure after the source commit but
before dispatch therefore loses the external effect permanently. Lowering a
webhook to the built-in `http_fetch` WASM module would inherit that loss window.

Such lowering would also bypass common `ActionTrigger` semantics. The current
custom-effect expansion is unconditional, so `to_state` and `guard` are not
applied to WASM, adapter, or webhook integrations. The trigger's optional
`principal` is not used for the source-trigger authorization decision; the WASM
host separately authorizes HTTP under its module principal. Treating those two
checks as interchangeable would violate ADR-0046.

The repository contains two other HTTP paths, neither of which closes this
gap:

- `temper-platform::integration` is an exported but unwired library subsystem.
  Production transitions do not feed it, and its retry/dead-letter state is
  in-memory rather than co-committed with entity state.
- `temper-server::webhooks` is an operator-configured trajectory subscriber.
  It intentionally operates outside IOA action-trigger semantics and remains a
  working, independently scoped capability.

Temper must not advertise an external effect that it cannot durably execute.
There are two correct end states: implement a canonical durable external-effect
runtime, or reject the declaration until that runtime exists. This ADR chooses
rejection because the existing execution paths cannot provide the required
contract without a new journaled delivery model.

## Decision

### 1. Reject every IOA-owned outbound webhook during validation

Validation returns a stable error for both outbound declaration forms:

- `[[action.triggers]] kind = "webhook"`; and
- legacy `[[integration]] type = "webhook"`, including the historical omitted
  `type` field that deserializes to `webhook`.

Rejection happens before action-trigger synthesis, transition-table
construction, registry installation, or action execution. No accepted
automaton may contain an `Integration { integration_type = "webhook" }` record,
and no custom trigger effect may be synthesized from a webhook action trigger.

`TriggerKind::Webhook` and its source fields remain deserializable so users get
an explicit, actionable validation error instead of an unknown-enum or ignored-
field error. The message states that IOA webhooks are unsupported until durable
delivery is available; it does not suggest a warning-only or best-effort mode.

This is not a compatibility mode. IOA webhook triggers have never executed in
the production runtime, so rejection removes a false promise rather than a
working capability. Existing specs that contain one fail installation and
verification instead of appearing healthy while dropping work.

A hand-authored `effect = "trigger <name>"` remains valid when it is not paired
with an explicitly declared webhook integration. `trigger` is the generic
custom-effect extension point and can have a registered runtime handler; its
syntax alone does not identify an HTTP effect. This ADR rejects only declarations
that the IOA schema explicitly classifies as outbound webhooks.

### 2. Keep working HTTP paths scoped to their existing contracts

The operator-level `webhooks.toml` trajectory subscriber remains supported. It
does not claim to execute IOA action triggers and must not be silently migrated
or removed.

The unwired `temper-platform::integration` engine API also remains in this
change. Although it is not connected to production IOA execution, callers can
construct its `IntegrationConfig` directly and its retry and dead-letter
behavior is tested. Removing it before there is a canonical runtime into which
that behavior can be migrated would drop working capability. Its tests no
longer use parsed IOA `[[integration]]` declarations as an ingestion path: they
either construct engine configuration directly or assert the IOA declaration
is rejected. The engine's existence does not make webhook IOA declarations
valid; conformance tests enforce that boundary.

No new direct HTTP dispatcher, WASM lowering, warning path, or background task
is introduced.

### 3. Define the acceptance gate for a future webhook runtime

Webhook declarations may be accepted again only when one canonical integration
runtime provides all of the following:

1. **Journaled intent.** The source transition and a bounded external-effect
   intent are committed atomically. The intent has a deterministic delivery ID
   derived from stable entity/event coordinates.
2. **Recovery and retry.** Replay reconstructs unfinished intents. A process
   failure at every boundary (before send, after send, before receipt commit)
   leaves either retryable work or a durable terminal receipt. At-least-once
   HTTP delivery uses the stable delivery ID as an idempotency key; exactly-once
   delivery is not claimed.
3. **Uniform trigger semantics.** `to_state`, every supported `guard`, declared
   or inherited `principal`, and `liveness` have the same meaning for entity,
   WASM, adapter, and webhook triggers. Guard-skipped work creates no delivery
   intent.
4. **Two authorization decisions.** Cedar first authorizes firing the trigger
   under its declared/inherited principal. The governed HTTP host separately
   authorizes network egress for the destination. Neither decision substitutes
   for the other.
5. **Durable outcomes.** Success, failure, retry exhaustion, callback dispatch,
   and compensation are observable and recoverable. Callback actions carry the
   stable delivery ID so replay cannot create unrelated callback attempts.
6. **Conformance.** Every accepted trigger kind has verifier/JIT/runtime tests,
   including restart, replay, deterministic ordering, authorization denial,
   non-2xx response, retry, and terminal failure cases.

This gate is a compatibility rule for the schema, not a deferred implementation
phase in this change. Until every item is implemented atomically, the parser is
the enforcement boundary.

## Rollout Plan

This ships in one pull request:

1. Add failing behavioral regressions proving both outbound webhook declaration
   forms are currently accepted even though no runtime consumes them.
2. Reject webhook action triggers and legacy webhook integrations, then assert
   that no later parser/JIT/runtime layer can observe an accepted webhook
   integration.
3. Add conformance coverage showing the remaining accepted trigger kinds still
   parse and preserve their existing expansion.
4. Update documentation that currently describes webhook triggers as accepted
   or parse-only.
5. Exercise verification and installation against a live local server: the
   pre-fix build accepts the broken spec; the fixed build rejects it before any
   source action can commit.

## Readiness Gates

- Webhook action triggers and legacy webhook integrations fail parsing with the
  stable unsupported-durable-delivery error.
- No accepted automaton can contain or synthesize a webhook integration, and no
  webhook action trigger can synthesize a custom effect.
- Entity, WASM, and adapter trigger parser conformance tests remain green.
- The operator trajectory webhook tests remain green.
- The live local server rejects installation of a webhook-bearing spec and
  performs no outbound request.
- Deterministic parser/JIT outputs for accepted trigger kinds are unchanged.
- `cargo fmt --check`, strict Clippy for touched crates, full workspace tests,
  reviewer PASS, Greptile, and CI are clean.

## Consequences

### Positive

- Verification and installation can no longer certify a webhook that runtime
  will silently drop.
- The fix does not add another HTTP, authorization, retry, or telemetry path.
- The durability and authorization requirements for future support are
  explicit and testable.
- Working operator webhooks and the tested platform integration library are not
  removed.

### Negative

- Specs containing `kind = "webhook"` fail until the journaled integration
  runtime exists.
- Users that mistook prior successful installation for webhook support must use
  an explicitly supported integration surface.

### Risks

- A caller could depend on parser acceptance even though delivery never
  occurred. The validation error is intentional: preserving silent loss would
  be a worse compatibility contract.
- Documentation or examples outside this repository may still claim webhook
  support. Repository search and release notes must make the rejection visible.

### DST Compliance

- Validation is a deterministic, pure function over the parsed automaton.
- The change adds no time source, randomness, thread, I/O, or unordered
  collection to simulation-visible crates.
- Replay behavior for accepted effects is unchanged; a rejected webhook can no
  longer enter the event journal.

## Non-Goals

- Implementing a partial or best-effort webhook dispatcher.
- Treating action idempotency as external-delivery idempotency.
- Treating post-execution invocation logging as an outbox.
- Removing or repurposing operator-level trajectory webhooks.
- Removing tested integration-library behavior before it can be migrated.
