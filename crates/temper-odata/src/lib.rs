//! temper-odata: OData v4 protocol implementation for Temper.
//!
//! Provides OData-compliant query parsing, response formatting,
//! and entity routing for Temper entity services.
//!
//! # Key modules
//!
//! - [`error`] — OData error types
//! - [`path`] — URL path parser
//! - [`query`] — Query options parser (`$filter`, `$select`, `$expand`, etc.)

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
