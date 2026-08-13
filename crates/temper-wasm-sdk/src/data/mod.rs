//! Typed application-data SDK contracts and guest wrappers (ADR-0157).

mod artifact;
mod client;
mod contracts;
mod manifest;
mod proof;
mod query_types;
mod response;

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
pub use proof::*;
pub use query_types::*;
pub use response::*;
