# ADR-0036: Governed Agent Architecture

## Status

Accepted

## Context

Proven open-source agent architectures already validate a useful set of patterns: append-only session trees, context compaction, a two-loop steering model, lazy skills, event streaming, and transport/channel adapters. The existing `TemperAgent` proves the basic governed loop, but it still stores flat conversation JSON, exposes only a poll-centric control plane, and keeps most capabilities inside a single agent/tool implementation boundary.

We want the Temper version of that architecture, but we do not want to wrap an external agent runtime as an opaque subprocess. The Temper runtime needs each capability to remain spec-driven, Cedar-governed, observable, and verifiable.

## Decision

Rebase `TemperAgent` onto these proven patterns and express the missing capabilities as governed Temper specs and WASM integrations:

- Session tree storage with JSONL append-only entries and branch tracking
- Explicit compaction and steering states in the TemperAgent IOA
- Soul, skill, memory, hook, heartbeat, and cron capabilities as first-class entities
- SSE-based lifecycle and progress streaming for entities
- Channel adapters and routing entities for multi-transport delivery
- Thin tool dispatch that executes sandbox tools directly and routes entity capabilities through OData

The `TemperAgent` remains the execution boundary, but the richer architecture is decomposed into separate governed entities instead of extending a monolithic match-arm tool runner.

## Alternatives Considered

1. Wrap an external agent runtime as a subprocess

Rejected. This would preserve the interaction semantics, but the actual runtime behavior would sit outside Temper governance, Cedar authorization, and IOA verification.

2. Build a new agent stack from scratch

Rejected. Existing agent architectures already validate the core interaction patterns we need. Re-learning those design choices inside a brand-new implementation adds unnecessary risk.

3. Extend the existing TemperAgent incrementally

Chosen. This keeps the proven Temper dispatch/runtime model while migrating the storage format, state machine, event transport, and capability surface toward the target architecture.

## Consequences

- `TemperAgent` conversation persistence changes from flat JSON to JSONL session-tree storage.
- New entity types are introduced in the `temper-agent` and `temper-channels` OS apps.
- Additional WASM modules are required for compaction, steering, heartbeat scanning, cron triggering, and channel routing.
- Event streaming becomes part of the agent contract instead of an optional side channel.
- Capability growth shifts from tool-runner branching to governed entity composition.
