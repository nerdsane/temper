//! OData error types for parsing and protocol errors.

use std::fmt;

/// Errors that can occur when parsing OData URLs and query options.
#[derive(Debug, Clone, PartialEq)]
pub enum ODataError {
    /// The URL path could not be parsed.
    InvalidPath { message: String },
    /// A query option could not be parsed.
    InvalidQueryOption { option: String, message: String },
    /// A $filter expression could not be parsed.
    InvalidFilter { message: String, position: usize },
    /// An unsupported query option was encountered.
    UnsupportedOption { option: String },
    /// A value could not be parsed (e.g. invalid GUID, date, number).
    InvalidValue { expected: String, found: String },
}

impl fmt::Display for ODataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ODataError::InvalidPath { message } => {
                write!(f, "Invalid OData path: {message}")
            }
            ODataError::InvalidQueryOption { option, message } => {
                write!(f, "Invalid OData query option '{option}': {message}")
            }
            ODataError::InvalidFilter { message, position } => {
                write!(f, "Invalid $filter at position {position}: {message}")
            }
            ODataError::UnsupportedOption { option } => {
                write!(f, "Unsupported OData query option: {option}")
            }
            ODataError::InvalidValue { expected, found } => {
                write!(f, "Invalid value: expected {expected}, found '{found}'")
            }
        }
    }
}

impl std::error::Error for ODataError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_invalid_path() {
        let err = ODataError::InvalidPath {
            message: "unexpected segment".into(),
        };
        assert_eq!(err.to_string(), "Invalid OData path: unexpected segment");
    }

    #[test]
    fn error_display_invalid_filter() {
        let err = ODataError::InvalidFilter {
            message: "expected operator".into(),
            position: 12,
        };
        assert_eq!(
            err.to_string(),
            "Invalid $filter at position 12: expected operator"
        );
    }
}
