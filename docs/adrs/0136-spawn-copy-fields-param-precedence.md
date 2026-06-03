# ADR-0136: Spawn Copy Fields Parameter Precedence

## Status

Accepted

## Context

Temper's `spawn` effect supports `copy_fields` so a parent entity can pass durable state into a child entity's initial action. The runtime captured copied field values in `SpawnRequest`, then merged them into the child initial action params after the parent action params.

That ordering made stale parent fields overwrite freshly computed callback params when a transition both recorded resolved values and spawned a child. A common example is a scheduler callback that computes `model` and `provider` as action params while the parent's previous `model` and `provider` fields are empty.

## Decision

Spawn initial action params are merged in this order:

1. copied parent state fields
2. explicit parent action params
3. platform-owned parent provenance fields (`parent_id`, `parent_type`, `<parent>_id`)

Explicit action params therefore win over copied state fields, while platform provenance remains authoritative and cannot be spoofed by caller input.

## Consequences

Declarative spawn flows can safely use `copy_fields` together with callback params that refresh or repair parent state in the same transition. Child initial actions receive the newly computed values instead of stale copied fields.

Existing flows that depended on `copy_fields` overriding explicit action params should instead omit those params from the parent action or use distinct field names. Parent provenance fields continue to override both sources.
