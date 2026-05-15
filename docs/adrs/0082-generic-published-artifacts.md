# ADR-0082: Generic Published Artifacts

Status: Accepted

## Context

TemperFS files are governed runtime resources. Public read-only surfaces often need
to serve selected file bytes at CDN/object-storage speed after an app-specific
publication decision has already been made.

Sending public traffic through `Files(...)/$value` keeps every read on the
governed materialization path. That path remains correct for private, draft, and
policy-sensitive reads, but it is the wrong hot path for immutable public bytes.

## Decision

Temper provides a generic `PublishedArtifact` primitive.

The primitive promotes an authorized governed file or file version into an
immutable public blob namespace and records rebuildable provenance:

- source file id
- optional source file version id
- content hash
- MIME type
- byte length
- public storage key
- public URL
- opaque owner reference
- opaque label

Temper does not interpret the label or owner reference. Applications decide what
their labels mean and which artifacts are required before publication.

## Consequences

- Temper stays app-agnostic: no application field names, lifecycle rules, or
  CDN path semantics live in the runtime.
- Public bytes can be served by object storage/CDN without re-entering Temper.
- Governed private reads still use TemperFS and may use the indexed `$value`
  fast path when projections are fresh.
- The `published_artifacts` table is a rebuildable read model, not the authority
  for whether an app entity is published.
