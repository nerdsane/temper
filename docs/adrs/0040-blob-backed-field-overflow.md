# ADR-0040: Blob-Backed Overflow for Large Entity Field Values

## Status

Proposed

## Context

Temper currently truncates oversized entity field values during field synchronization. This protects WASM context size, but it also destroys high-value runtime data such as long agent outputs before downstream consumers can read it.

OpenPaw can mitigate some of the user-facing impact by reading recent event params and degrading Discord delivery more carefully, but that does not solve the platform-level problem:

- OData consumers still see truncated field data
- any app storing meaningful large values in entity fields inherits the same failure mode
- each app would otherwise need to invent its own side channel or filesystem workaround

Temper already has a content-addressed blob store used by the filesystem layer. That storage model is a better fit for oversized entity values than silent truncation.

## Decision

When a serialized field value exceeds the entity field size limit, Temper should:

1. write the serialized value to the blob store under a dedicated overflow bucket
2. store a small reference object in the entity field, for example:

```json
{"__blob_ref":"<hash>","size":123456}
```

3. resolve that reference transparently on entity read paths

Resolution should happen in the same places clients already read entity state, including OData entity reads and query/property resolution.

## Rationale

- This keeps entity semantics intact: callers still read the full logical field value.
- It reuses an existing platform primitive instead of adding another storage abstraction.
- It removes the need for every OS app to hand-roll its own large-payload workaround.

## Consequences

### Positive

- Large agent results and other oversized state survive platform round-trips.
- OData and Observe callers continue to see complete values.
- App authors do not need to special-case large output fields.

### Negative

- Entity reads for overflowed fields now depend on blob availability.
- The read path gains extra complexity and an additional fetch step.

## Rollout Notes

- The write path should remain deterministic by deriving blob keys from content.
- Missing blobs should degrade safely by returning the stored reference object and logging the loss.
- OpenPaw's delivery hardening can stay in place as a defense-in-depth measure even after this lands.
