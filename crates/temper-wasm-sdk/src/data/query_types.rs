use serde::{Deserialize, Serialize};

/// Stable query ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderV1 {
    /// Canonical property name.
    pub field: String,
    /// Requested sort direction.
    pub direction: OrderDirectionV1,
}

/// Query sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderDirectionV1 {
    /// Ascending, with nulls last.
    Asc,
    /// Descending, with nulls first.
    Desc,
}

/// Bounded page request. Cursors are opaque host output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageV1 {
    /// Maximum values returned in this page.
    pub limit: u32,
    /// Opaque cursor returned by an identical prior query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}
