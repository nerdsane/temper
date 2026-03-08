//! OData v4 protocol implementation: path parsing, query options, and error types.

pub mod error;
pub mod path;
pub mod query;

// Re-export primary types for convenience.
pub use error::ODataError;
pub use path::{KeyValue, ODataPath, parse_path};
pub use query::{
    BinaryOperator, ExpandItem, ExpandOptions, FilterExpr, ODataValue, OrderByClause,
    OrderDirection, QueryOptions, UnaryOperator, parse_filter, parse_query_options,
};
