//! OData handler modules.

mod account_verification;
mod app_uniqueness;
pub(crate) mod authz;
mod bindings;
mod blob_media;
pub(crate) mod common;
pub(crate) mod constraints;
mod content_addressed;
mod filter_sql;
mod nearest;
mod query_plane_read;
pub(crate) mod rate_limit;
mod read;
mod read_support;
mod response;
mod schema_pin;
mod storage_guardrails;
mod stream_fast_path;
mod stream_put;
mod write;

pub use content_addressed::handle_blob_ingest_raw;
pub use read::handle_hints;
pub use read::handle_metadata;
pub use read::handle_odata_get;
pub use read::handle_service_document;
pub use write::handle_odata_delete;
pub use write::handle_odata_patch;
pub use write::handle_odata_post;
pub use write::handle_odata_put;
