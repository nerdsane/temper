//! Catalog rows of one entity type taken together, as a store returns them.

/// Catalog rows of one entity type that hold the same status and the same
/// value of each field asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogGroupRow {
    /// Their status.
    pub status: String,
    /// The value of each field asked for, in the order asked; `None` when
    /// the rows do not hold the field.
    pub values: Vec<Option<serde_json::Value>>,
    /// Their entity ids, in order.
    pub ids: Vec<String>,
}
