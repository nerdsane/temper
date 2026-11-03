//! OData v4 query options parser.
//!
//! Parses `$filter`, `$select`, `$expand`, `$orderby`, `$top`, `$skip`, and `$count`
//! from a URL query string, or from already-decoded key/value pairs, into a
//! structured [`QueryOptions`].

pub mod filter;
mod query_string;
pub mod types;

pub use filter::parse_filter;
pub use types::{
    BinaryOperator, ExpandItem, ExpandOptions, FilterExpr, ODataValue, OrderByClause,
    OrderDirection, QueryOptions, UnaryOperator, parse_query_options,
    parse_query_options_from_pairs,
};
