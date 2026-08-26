//! Bounded, versioned application-facing failure contracts.
//!
//! This crate owns data contracts only. Error-source adapters stay with the
//! crate that owns each source error so classification never depends on a
//! human-readable display string.

mod bounds;
mod declaration;
mod details;
mod envelope;
mod error;
mod token;

pub use bounds::{
    FAILURE_ENVELOPE_CSDL_TYPE_V1, FAILURE_ENVELOPE_VERSION_V1, MAX_DETAIL_ENTRIES,
    MAX_DETAIL_KEY_BYTES, MAX_DETAIL_STRING_BYTES, MAX_DETAILS_SERIALIZED_BYTES,
    MAX_DIAGNOSTIC_BYTES, MAX_FAILURE_CODE_BYTES, MAX_OPERATION_ATTEMPT, MAX_OPERATION_ID_BYTES,
    MAX_OPERATION_KIND_BYTES, MAX_PROVENANCE_TOKEN_BYTES,
};
pub use declaration::GuestFailureDeclarationV1;
pub use details::{BoundedFailureDetails, FailureDetailValue};
pub use envelope::{
    CausalOperationV1, FailureCategory, FailureEnvelopeV1, FailureOutcome, FailureProvenanceV1,
    FailureRetryability, FailureSource, OperationAttempt,
};
pub use error::FailureContractError;
pub use token::{
    BoundedDetailString, BoundedDiagnostic, DetailKey, OperationId, OperationKind, ProvenanceToken,
    StableFailureCode,
};

#[cfg(test)]
mod tests;
