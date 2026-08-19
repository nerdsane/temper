//! Query-plane replay-parity report types.

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct QueryProjectionReplayParityReport {
    /// Tenant whose persisted journals and projection rows were compared.
    pub tenant: String,
    /// Optional entity type scope applied to this verifier run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<String>,
    /// Optional maximum number of entities considered by this verifier run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_limit: Option<u64>,
    /// Number of persisted entities considered by the verifier.
    pub checked: u64,
    /// Number of non-deleted entities whose projection row matched replayed state.
    pub matched: u64,
    /// Number of entities whose projection row diverged from replayed state.
    pub drifted: u64,
    /// Number of active entities missing from the projection catalog.
    pub missing: u64,
    /// Number of replayed deleted entities correctly absent from the catalog.
    pub deleted_absent: u64,
    /// Number of entities the verifier could not compare because of a store or spec error.
    pub errors: u64,
    /// Bounded sample of drift/error examples for operator diagnosis.
    pub drift_examples: Vec<QueryProjectionReplayParityDrift>,
}

impl QueryProjectionReplayParityReport {
    /// Returns true when every checked entity matched the projection contract.
    pub fn is_clean(&self) -> bool {
        self.drifted == 0 && self.missing == 0 && self.errors == 0
    }
}

/// One bounded replay parity drift or error example.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct QueryProjectionReplayParityDrift {
    /// Entity type whose projection diverged from replayed state.
    pub entity_type: String,
    /// Entity identifier for targeted repair. This is intentionally not emitted as a metric tag.
    pub entity_id: String,
    /// Drift class, such as `fields`, `sequence`, `missing_catalog`, or `deleted_present`.
    pub drift_kind: String,
    /// Sequence relationship between catalog and replayed state.
    pub sequence_direction: String,
    /// Absolute sequence gap when both sides have sequence numbers.
    pub sequence_gap: u64,
    /// Sequence number currently stored in the projection catalog, when a row exists.
    pub catalog_sequence: Option<u64>,
    /// Sequence number recovered from authoritative event replay.
    pub authoritative_sequence: u64,
}
