//! OData handler modules.

mod account_verification;
mod app_uniqueness;
mod bindings;
mod common;
pub(crate) mod constraints;
mod content_addressed;
mod filter_sql;
mod rate_limit;
mod read;
mod read_support;
mod response;
mod storage_guardrails;
mod stream_fast_path;
mod stream_put;
mod write;

#[cfg(feature = "observe")]
pub(crate) use common::extract_tenant;
pub use content_addressed::handle_blob_ingest_raw;
pub use read::handle_hints;
pub use read::handle_metadata;
pub use read::handle_odata_get;
pub use read::handle_service_document;
pub use write::handle_odata_delete;
pub use write::handle_odata_patch;
pub use write::handle_odata_post;
pub use write::handle_odata_put;
