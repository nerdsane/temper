# ADR-0085: Published Artifacts Postgres Metadata

Status: Accepted

## Context

ADR-0173 introduced generic published artifacts as a rebuildable read model for
public TemperFS bytes. The implementation persisted that read model through the
Turso store path, but production now runs on the Postgres storage stack. Live
TemperPaw verification on 2026-05-13 showed `POST /api/files/publish-artifact`
returning HTTP 200 while emitting
`published artifact metadata store unavailable; returning derived artifact row`.

That fallback is observable, but it is not an acceptable steady state: the route
can write public bytes and return a derived response while losing the durable
publication metadata that humans and agents need for audit, recovery, and
debugging.

## Decision

Published-artifact metadata is a backend-neutral metadata-store capability.

- Postgres owns a `published_artifacts` table with the same logical fields as
  the Turso read model.
- `MetadataStore` exposes explicit upsert/load methods for published artifacts
  instead of routing business logic through `turso_store_for_tenant`.
- The publish route must persist through `metadata_store_for_tenant`, so
  Postgres production and Turso development deployments share the same behavior.
- Store implementations must emit normal database spans through their backend
  query wrappers, preserving the publish trace shape:
  `state.publish_file_artifact -> state.read_file_stream_indexed`,
  `state.put_public_blob`, and the published-artifact metadata write.
- Successful publishes must emit durable, Datadog-queryable publication
  metadata on the route span, the state span, and the success log:
  `artifact_id`, `source_file_id`, `source_file_version_id`, `content_hash`,
  `artifact_label`, `mime_type`, `byte_length`, `public_storage_key`,
  `public_url`, `owner_ref_type`, `owner_ref_id`, `artifact_status`,
  `metadata_backend`, `artifact_namespace`, `public_blob_bucket`, and
  `public_blob_endpoint_host`. These fields are not incidental debug strings;
  they are the contract humans and agents use to audit a public data-doc
  publication from request through blob upload and metadata persistence.

## Consequences

- Postgres production no longer silently degrades to a derived-only artifact row.
- Published artifact metadata remains a rebuildable read model, not the source
  of truth for application publication decisions.
- The Postgres schema grows by one tenant-scoped table and two indexes.
- Existing derived-only responses cannot be recovered unless their public object
  storage keys are known; future publishes persist metadata durably.

## Verification

- Unit/schema tests prove Postgres migrations and schema constants include the
  published-artifacts table and tenant RLS policy.
- Postgres store tests prove upsert, update, and load behavior matches the
  Turso implementation.
- Server tests prove `publish_file_artifact` persists through the backend-neutral
  metadata store instead of the Turso-only path.
- Unit tests prove the successful publication log includes the public blob and
  persisted artifact identifiers needed for Datadog search and agent-readable
  diagnostics.
- Live TemperPaw proof must show the warning is gone for a real
  `POST /api/files/publish-artifact` trace on the Postgres production path and
  that the success log exposes the same artifact/public-blob identifiers as the
  HTTP response.
