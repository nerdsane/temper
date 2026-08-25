# ADR-0183: Restore Canonical Module Data Bindings

- Status: Accepted
- Date: 2026-08-25
- Deciders: Temper core maintainers
- Related:
  - ADR-0157: Metadata-Generated Typed Module Data SDK
  - ADR-0180: Local-First Immutable App Bundles
  - ADR-0182: App-Rooted Module Binding Verification
  - `crates/temper-platform/src/os_apps/reconcile.rs`

## Context

Typed module data grants are verified from the canonical bundle and attached to
the in-memory WASM registry when an OS app installs its modules. The module
artifact and its source metadata are durable, but the verified binding map is
intentionally host-only and is rebuilt after process restart.

Cache reconciliation can prove that the installed bundle digest, registered
module digest, durable module digest, specifications, and policies are already
current. It then skips the install pipeline. Because binding activation was
inside the WASM-install phase, an unchanged module recovered from durable state
had no typed-data binding. Its first host call therefore failed closed with a
zero request budget even though the canonical artifact and grant were valid.

## Decision

Canonical reconciliation restores verified typed-data bindings whenever the
exact bundle artifacts are both registered in memory and present in durable
storage. This restoration happens before the unchanged-bundle fast path and
before delta planning, independently of whether the WASM install phase runs.

The reconciler binds only manifests freshly verified from the materialized
canonical bundle. It recomputes each artifact digest and requires that digest
to equal the currently registered module digest before attaching the grant.
Missing or mismatched artifacts fail reconciliation closed.

The durable module row remains the source for recovering executable bytes; the
canonical bundle remains the source for recovering capability metadata. No
grant is persisted or inferred from mutable tenant state.

## Consequences

- Cache-only restart restores typed-data clients without reinstalling unchanged
  module artifacts.
- Spec-, policy-, content-, or seed-only delta reconciliation cannot silently
  drop an unchanged module's data grant.
- Recovery repeats bounded binding verification and digest checks during
  canonical reconciliation.
- Non-canonical local installs retain their existing activation path and do not
  gain inferred bindings.

## Verification

A regression test registers the exact recovered artifact without a binding,
restores its verified canonical manifest, and asserts the host registry exposes
the binding for that tenant, module, and artifact digest.

## DST Compliance

This change affects startup reconciliation outside simulation-visible code.
Iteration uses canonical `BTreeMap` order and introduces no time, randomness,
or scheduling dependency.

## Alternatives Considered

1. **Force every cache restart through WASM reinstall** — Rejected because it
   discards the existing delta-reconcile optimization and conflates artifact
   persistence with host-only capability activation.
2. **Persist the verified grant beside the module row** — Rejected because the
   canonical bundle already provides the signed, reproducible verification
   inputs and a second durable representation could drift.
3. **Lazily attach a grant on first host call** — Rejected because invocation
   should consume an already activated capability, not perform deployment work.

## Rollback Policy

Revert the pre-skip restoration call and helper together. No stored schema or
entity data requires migration, but typed canonical modules will again fail
after an unchanged cache restart until their WASM phase is reinstalled.
