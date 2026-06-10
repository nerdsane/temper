//! PostgreSQL platform-store methods, split by domain.
//!
//! Each submodule holds one domain's `impl PostgresEventStore` block plus the
//! row types and row-mapping helpers that belong to it, mirroring the layout
//! of `temper-store-turso/src/store/`. Shared parsing and hashing helpers
//! live in this module; every public item is re-exported here so callers keep
//! importing from `crate::platform::*` (and `temper_store_postgres::*` via
//! the lib.rs re-exports).

mod blobs;
mod evolution;
mod ots;
mod policies;
mod projection;
mod published_artifacts;
mod secrets;
mod specs;
mod trajectory;
mod wasm;

pub use evolution::{
    PostgresDesignTimeEventRow, PostgresEvolutionRecordRow, PostgresFeatureRequestRow,
};
pub use ots::{PostgresOtsTrajectoryParams, PostgresOtsTrajectoryRow};
pub use policies::{PostgresPolicyDenialPatternRow, PostgresPolicyRow};
pub use projection::{PostgresEntityCatalogRow, PostgresProjectedEntityFieldsRow};
pub use published_artifacts::{PostgresPublishedArtifactRow, PostgresPublishedArtifactUpsert};
pub use secrets::PostgresSecretRow;
pub use specs::{PostgresInstalledAppRow, PostgresSpecRow, PostgresSpecVerificationUpdate};
pub use trajectory::{
    PostgresActionStats, PostgresAgentSummary, PostgresTrajectoryInsert, PostgresTrajectoryRow,
    PostgresTrajectoryStats, PostgresUnmetIntentAggRow,
};
pub use wasm::{
    PostgresWasmInvocationInsert, PostgresWasmInvocationRow, PostgresWasmModuleMetadataRow,
    PostgresWasmModuleRow,
};

pub(crate) use projection::scalar_index_fields;

use sha2::{Digest, Sha256};
use temper_runtime::persistence::PersistenceError;

pub(crate) fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}

fn parse_json(data: &str) -> Result<serde_json::Value, PersistenceError> {
    serde_json::from_str(data).map_err(|e| PersistenceError::Serialization(e.to_string()))
}

fn parse_optional_json(data: Option<&str>) -> Result<Option<serde_json::Value>, PersistenceError> {
    data.map(parse_json).transpose()
}

fn parse_rfc3339(value: &str) -> Result<chrono::DateTime<chrono::Utc>, PersistenceError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| PersistenceError::Serialization(e.to_string()))
}

fn parse_optional_rfc3339(
    value: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
    value.map(parse_rfc3339).transpose()
}

pub(crate) fn json_hash(value: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}
