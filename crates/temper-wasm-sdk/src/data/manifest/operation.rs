//! Closed application-data operation names accepted in `app.toml`.

use serde::{Deserialize, Serialize};

/// Closed operation names accepted in `app.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataOperationKind {
    /// Read one entity.
    EntityGet,
    /// Query an entity collection.
    EntityQuery,
    /// Create one entity.
    EntityCreate,
    /// Patch one entity.
    EntityPatch,
    /// Invoke one bound action.
    ActionInvoke,
    /// Execute a bounded non-atomic batch.
    Batch,
    /// Invoke one verified composite action.
    CompositeInvoke,
    /// Read file metadata or content.
    FileRead,
    /// Write file content.
    FileWrite,
    /// Submit an immutable schema bundle.
    SchemaBundleSubmit,
    /// Read an immutable schema bundle.
    SchemaBundleGet,
    /// Verify a submitted schema bundle.
    SchemaBundleVerify,
    /// Activate a verified schema bundle.
    SchemaBundleActivate,
    /// Retire an inactive schema bundle.
    SchemaBundleRetire,
    /// Start a durable schema migration.
    SchemaMigrationStart,
    /// Read a schema migration job.
    SchemaMigrationGet,
    /// Retry a failed schema migration.
    SchemaMigrationRetry,
    /// Create a governed stream-descriptor migration job.
    StreamDescriptorMigrationStart,
    /// Advance one bounded stream-descriptor inventory page.
    StreamDescriptorMigrationAdvance,
    /// Read stream-descriptor migration progress.
    StreamDescriptorMigrationGet,
    /// Read redacted unresolved stream-descriptor classifications.
    StreamDescriptorMigrationListUnresolved,
}
