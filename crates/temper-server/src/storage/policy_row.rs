use temper_store_postgres::PostgresPolicyRow;
use temper_store_turso::PolicyRow as TursoPolicyRow;

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
