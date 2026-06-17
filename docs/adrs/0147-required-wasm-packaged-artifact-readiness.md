# ADR-0147: Required WASM Packaged Artifact Readiness

## Status

Accepted

## Context

OS app reconcile compares the installed app digest with the currently loaded bundle and skips when specs, policies, and WASM appear ready. Required WASM modules declared in `app.toml` were only checked after artifacts were discovered. If discovery found no bytes for a configured required module, readiness over the empty discovered set could incorrectly pass and leave an older durable or in-memory module active.

WASM discovery also preferred `target/` build outputs before the packaged sibling `.wasm` copied next to each module for deployment. That made local/deploy verification less direct when both a stale target output and a packaged artifact existed.

## Decision

Required manifest-declared WASM modules must have bundled artifact bytes before registry or durable WASM readiness can pass. Missing optional artifacts remain skippable.

WASM discovery now prefers packaged sibling artifacts before falling back to target release outputs. Target output remains a local development fallback when no packaged artifact exists.

## Consequences

Reconcile will run instead of silently skipping when a required module artifact is missing. The install step already records required missing artifacts as WASM failures, so operators see an explicit failure instead of stale module reuse.

Packaged `.wasm` files are the deploy source of truth for bundle hashing and install, matching Docker target-pruning behavior and CI package verification.
