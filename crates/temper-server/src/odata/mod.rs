//! OData handler modules.

mod bindings;
mod common;
pub(crate) mod constraints;
mod read;
mod response;
mod write;

#[cfg(feature = "observe")]
pub(crate) use common::extract_tenant;
pub use read::handle_hints;
pub use read::handle_metadata;
pub use read::handle_odata_get;
pub use read::handle_service_document;
pub use write::handle_odata_delete;
pub use write::handle_odata_patch;
pub use write::handle_odata_post;
pub use write::handle_odata_put;
