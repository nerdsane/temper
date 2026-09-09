//! Typed write inputs for atomic platform-store operations.

/// Values written atomically as one evolution record.
#[derive(Clone, Copy, Debug)]
pub struct PostgresEvolutionRecordInsert<'a> {
    /// Tenant that owns the record.
    pub tenant: &'a str,
    /// Stable evolution record identifier.
    pub id: &'a str,
    /// Evolution record kind.
    pub record_type: &'a str,
    /// Current record status.
    pub status: &'a str,
    /// Principal that created the record.
    pub created_by: &'a str,
    /// Optional predecessor record identifier.
    pub derived_from: Option<&'a str>,
    /// Serialized record payload.
    pub data_json: &'a str,
}

/// Exact decision and policy values committed by one approval transaction.
#[derive(Clone, Copy, Debug)]
pub struct PostgresPolicyApprovalCommit<'a> {
    /// Tenant that owns both rows.
    pub tenant: &'a str,
    /// Pending decision to transition.
    pub decision_id: &'a str,
    /// Serialized approved decision.
    pub approved_decision_json: &'a str,
    /// Policy row created by the decision.
    pub policy_id: &'a str,
    /// Approved Cedar source.
    pub cedar_text: &'a str,
    /// Principal that approved the decision.
    pub created_by: &'a str,
}
