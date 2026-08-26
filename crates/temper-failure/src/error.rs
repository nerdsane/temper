//! Contract validation errors.

/// A v1 failure contract violated a declared wire budget or invariant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FailureContractError {
    /// A required ASCII token was empty.
    #[error("{field} must not be empty")]
    EmptyToken {
        /// Contract field name.
        field: &'static str,
    },
    /// A token contained a character outside the v1 allowlist.
    #[error("{field} contains invalid byte at index {index}")]
    InvalidTokenByte {
        /// Contract field name.
        field: &'static str,
        /// Zero-based byte index.
        index: usize,
    },
    /// A string field exceeded its byte budget.
    #[error("{field} uses {actual} bytes, exceeding the {max}-byte budget")]
    FieldTooLong {
        /// Contract field name.
        field: &'static str,
        /// Maximum allowed UTF-8 bytes.
        max: usize,
        /// Actual UTF-8 bytes.
        actual: usize,
    },
    /// An operation attempt exceeded the v1 budget.
    #[error("operation attempt {actual} exceeds the maximum {max}")]
    AttemptOutOfRange {
        /// Maximum supported attempt.
        max: u16,
        /// Rejected attempt.
        actual: u16,
    },
    /// A details map exceeded its entry budget.
    #[error("failure details contain {actual} entries, exceeding the maximum {max}")]
    TooManyDetails {
        /// Maximum supported entries.
        max: usize,
        /// Rejected entry count.
        actual: usize,
    },
    /// A details map exceeded its complete serialized byte budget.
    #[error("failure details serialize to {actual} bytes, exceeding the maximum {max}")]
    DetailsTooLarge {
        /// Maximum supported bytes.
        max: usize,
        /// Rejected serialized bytes.
        actual: usize,
    },
    /// Safe scalar detail serialization failed.
    #[error("failure details could not be encoded: {0}")]
    DetailsEncoding(String),
    /// A wire envelope named an unsupported version.
    #[error("unsupported failure envelope version {actual}; expected {expected}")]
    UnsupportedVersion {
        /// Supported v1 value.
        expected: u16,
        /// Rejected wire value.
        actual: u16,
    },
    /// An ambiguous failure claimed a known outcome.
    #[error("ambiguous failure category requires an unknown outcome")]
    AmbiguousCategoryWithKnownOutcome,
    /// Unknown outcome and retry guidance disagreed.
    #[error(
        "unknown outcomes require reconcile retryability, and reconcile requires unknown outcome"
    )]
    InvalidReconciliationGuidance,
    /// An omission marker contradicted the corresponding optional field.
    #[error("{field} must be absent when its omission marker is true")]
    ContradictoryOmission {
        /// Optional field with contradictory wire state.
        field: &'static str,
    },
}
