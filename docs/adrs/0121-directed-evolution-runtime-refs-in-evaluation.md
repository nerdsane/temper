# ADR 0121: Directed Evolution Runtime Refs In Evaluation

## Status

Accepted

## Context

Directed Evolution variants are meant to become live hot-loaded Genesis app
refs in isolated tenants before simulated users and reviewers evaluate them.
The current evaluation prompt includes the variant `AppRef`, but not the
runtime ref that tells the brain which live tenant/app installation to exercise.
That would let an evaluator inspect a candidate description without necessarily
using the running variant.

## Decision

Evaluation work item prompts must include the variant `RuntimeRef` recorded on
the `Variant` entity and a public Temper API base URL. For hot-loaded variants
the runtime ref has the shape
`temper://tenant/<tenant>/app/<owner/name@hash>`.

The app uses two API URL configs:

- `temper_api_url` remains the loopback URL used by WASM modules for their own
  governed host HTTP calls.
- `temper_public_api_url` is copied into brain prompts so local/background
  Codex workers can call the deployed Temper API.

The evaluation brain remains external to the app state machine, but the state
machine supplies the exact live runtime address and API access instructions as
part of the governed WorkItem prompt.

## Consequences

- Simulated user and reviewer brains can exercise the concrete variant tenant.
- The UI can continue to display `RuntimeRef` as evidence that a variant is
  running, not just committed.
- Evaluation prompts fail softer for older variants: if no `RuntimeRef` is
  recorded, the prompt includes an empty value rather than inventing one.
