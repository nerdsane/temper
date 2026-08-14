//! Backend identity and backend-neutral policy rows.

use temper_runtime::persistence::{PersistenceEnvelope, PersistenceError};
use temper_store_postgres::PostgresPolicyRow;
use temper_store_turso::PolicyRow as TursoPolicyRow;

/// Backend label used for metrics and operator-facing diagnostics only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendLabel {
    Postgres,
    Turso,
    Redis,
    TursoRouted,
    Sim,
}

impl BackendLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Turso => "turso",
            Self::Redis => "redis",
            Self::TursoRouted => "turso-routed",
            Self::Sim => "sim",
        }
    }
}

/// Backend-neutral row for one granular Cedar policy entry.
#[derive(Clone, Debug)]
pub struct PolicyStoreRow {
    pub tenant: String,
    pub policy_id: String,
    pub cedar_text: String,
    pub policy_hash: String,
    pub created_at: String,
    pub created_by: String,
    pub enabled: bool,
}

impl From<TursoPolicyRow> for PolicyStoreRow {
    fn from(row: TursoPolicyRow) -> Self {
        Self {
            tenant: row.tenant,
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            policy_hash: row.policy_hash,
            created_at: row.created_at,
            created_by: row.created_by,
            enabled: row.enabled,
        }
    }
}

impl From<PostgresPolicyRow> for PolicyStoreRow {
    fn from(row: PostgresPolicyRow) -> Self {
        Self {
            tenant: row.tenant,
            policy_id: row.policy_id,
            cedar_text: row.cedar_text,
            policy_hash: row.policy_hash,
            created_at: row.created_at,
            created_by: row.created_by,
            enabled: row.enabled,
        }
    }
}

/// Inputs for a native brand-new data-only entity create.
pub struct DataOnlyCreateRecord<'a> {
    /// Tenant that owns the entity.
    pub tenant: &'a str,
    /// Entity type being created.
    pub entity_type: &'a str,
    /// Entity identifier.
    pub entity_id: &'a str,
    /// Initial status.
    pub status: &'a str,
    /// Projection fields.
    pub fields: &'a serde_json::Value,
    /// Full response state.
    pub state: &'a serde_json::Value,
    /// First event envelope.
    pub event: &'a PersistenceEnvelope,
}

/// Optional native storage capability for brand-new data-only creates.
#[async_trait::async_trait]
pub trait DataOnlyCreateStore: Send + Sync {
    /// Persist the first event and initial projection atomically.
    async fn create_data_only_entity(
        &self,
        record: DataOnlyCreateRecord<'_>,
    ) -> Result<u64, PersistenceError>;
}
