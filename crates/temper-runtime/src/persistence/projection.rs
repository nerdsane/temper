//! Small journal/catalog checkpoints for restart repair, without entity bodies.

/// The last durable sequence of one record, including compacted snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalHead {
    /// Record type.
    pub entity_type: String,
    /// Record id within the tenant and type.
    pub entity_id: String,
    /// Highest durable sequence.
    pub sequence_nr: u64,
    /// A terminal kernel tombstone is present in the journal.
    pub deleted: bool,
}

/// The catalog's effective checkpoint, including unchanged-projection counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCheckpoint {
    /// Record type.
    pub entity_type: String,
    /// Record id within the tenant and type.
    pub entity_id: String,
    /// Effective projected sequence.
    pub sequence_nr: u64,
    /// Stored state name.
    pub status: String,
}

/// A missing or lagging catalog row. Only these records need replay at boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionRepairCandidate {
    /// Journal checkpoint observed by the comparison.
    pub journal: JournalHead,
    /// Previous catalog checkpoint, absent for an interrupted create.
    pub projection: Option<ProjectionCheckpoint>,
}
