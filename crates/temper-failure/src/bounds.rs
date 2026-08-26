//! V1 wire-contract budgets.

/// Serialized version discriminant for [`crate::FailureEnvelopeV1`].
pub const FAILURE_ENVELOPE_VERSION_V1: u16 = 1;
/// Canonical CSDL type name used by generated SDK callback parameters.
pub const FAILURE_ENVELOPE_CSDL_TYPE_V1: &str = "Temper.FailureEnvelopeV1";
/// Maximum UTF-8 byte length of a stable failure code.
pub const MAX_FAILURE_CODE_BYTES: usize = 64;
/// Maximum UTF-8 byte length of an operation or parent-operation identifier.
pub const MAX_OPERATION_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length of an operation kind.
pub const MAX_OPERATION_KIND_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a provenance component or source code.
pub const MAX_PROVENANCE_TOKEN_BYTES: usize = 64;
/// Maximum operation attempt represented by v1.
pub const MAX_OPERATION_ATTEMPT: u16 = 1_024;
/// Maximum UTF-8 byte length of an optional diagnostic message.
pub const MAX_DIAGNOSTIC_BYTES: usize = 512;
/// Maximum number of safe scalar detail entries.
pub const MAX_DETAIL_ENTRIES: usize = 16;
/// Maximum UTF-8 byte length of a detail key.
pub const MAX_DETAIL_KEY_BYTES: usize = 64;
/// Maximum UTF-8 byte length of a detail string value.
pub const MAX_DETAIL_STRING_BYTES: usize = 256;
/// Maximum serialized JSON bytes of the complete details object.
pub const MAX_DETAILS_SERIALIZED_BYTES: usize = 2_048;
