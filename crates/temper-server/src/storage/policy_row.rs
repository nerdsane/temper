use temper_store_postgres::PostgresPolicyRow;
use temper_store_turso::PolicyRow as TursoPolicyRow;

/// Backend-neutral row for one granular Cedar policy entry.
#[derive(Clone, Debug)]
pub struct PolicyStoreRow {
    /// Tenant that owns the policy.
    pub tenant: String,
    /// Stable policy identifier within the tenant.
    pub policy_id: String,
    /// Cedar policy source text.
    pub cedar_text: String,
    /// Content hash used for policy version checks.
    pub policy_hash: String,
    /// Backend-provided creation timestamp.
    pub created_at: String,
    /// Principal that created the policy.
    pub created_by: String,
    /// Whether the policy participates in authorization decisions.
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
