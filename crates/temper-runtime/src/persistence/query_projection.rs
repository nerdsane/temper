//! Backend-neutral query-projection ordering.

/// One host-resolved query-projection order target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueryProjectionOrderTarget {
    /// One canonical projected entity property.
    Property(String),
    /// Canonical entity status.
    Status,
    /// Canonical entity identifier.
    EntityId,
    /// Host-owned entity commit sequence.
    EntityCommitSequence,
}

/// One ordered query-projection clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryProjectionOrder {
    /// Typed target resolved by the host.
    pub target: QueryProjectionOrderTarget,
    /// Whether the target is ordered descending.
    pub descending: bool,
}
