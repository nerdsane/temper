//! Typed application-data SDK contracts and guest wrappers (ADR-0157).

mod artifact;
mod client;
mod contracts;
mod manifest;
mod module_data_failure;
mod proof;
mod query_types;
mod response;
#[cfg(all(not(target_arch = "wasm32"), feature = "test-helpers"))]
mod test_host;

pub use artifact::{
    ArtifactModuleSdkBinding, bind_module_sdk_artifact, read_module_sdk_artifact_binding,
};
pub use client::{
    DataClient, FileReader, FileWriter, OpenedFileRead, TypedAction, TypedEntity, TypedPage,
    TypedWrite, decode_action, decode_entity, decode_file_read, decode_file_write, decode_page,
    decode_write,
};
pub use contracts::*;
pub use manifest::*;
pub use module_data_failure::adapt_module_data_error;
pub use proof::*;
pub use query_types::*;
pub use response::*;
pub use temper_failure::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "test-helpers"))]
pub use test_host::{install_native_data_host_for_test, take_native_data_requests_for_test};
