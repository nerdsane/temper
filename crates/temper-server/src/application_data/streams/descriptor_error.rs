//! Stable typed SDK error mapping for descriptor admission.

use temper_wasm_sdk::data::{ModuleDataError, ModuleDataErrorKind};

use crate::application_data::data_error;
use crate::state::StreamDescriptorResolutionError;

pub(super) fn invalid_stream() -> ModuleDataError {
    data_error(
        ModuleDataErrorKind::InvalidRequest,
        "InvalidFileStream",
        "File stream handle is invalid or has the wrong direction",
    )
}

pub(super) fn stream_registry_unavailable() -> ModuleDataError {
    data_error(
        ModuleDataErrorKind::Internal,
        "InvocationStatePoisoned",
        "File stream registry unavailable",
    )
}

pub(super) fn stream_descriptor_error(error: StreamDescriptorResolutionError) -> ModuleDataError {
    let stable_code = error.stable_code();
    match error {
        StreamDescriptorResolutionError::BudgetExceeded => data_error(
            ModuleDataErrorKind::BudgetExceeded,
            stable_code,
            "File content exceeds the stream byte budget",
        ),
        StreamDescriptorResolutionError::Missing => data_error(
            ModuleDataErrorKind::ConsistencyUnavailable,
            stable_code,
            "Authoritative stream descriptor is unavailable",
        ),
        StreamDescriptorResolutionError::Integrity(_) => data_error(
            ModuleDataErrorKind::ConsistencyUnavailable,
            stable_code,
            "Committed stream content failed integrity verification",
        ),
        StreamDescriptorResolutionError::ReplayBudgetExceeded
        | StreamDescriptorResolutionError::Consistency(_) => data_error(
            ModuleDataErrorKind::ConsistencyUnavailable,
            stable_code,
            "Authoritative stream descriptor is inconsistent",
        ),
        StreamDescriptorResolutionError::JournalUnavailable
        | StreamDescriptorResolutionError::Storage(_) => data_error(
            ModuleDataErrorKind::TransientUnavailable,
            stable_code,
            "Authoritative stream descriptor storage is unavailable",
        ),
    }
}
