//! Compatibility facade for OData dispatch handlers.
//!
//! The implementation lives under [`crate::odata`]. This module keeps
//! existing imports stable while the OData stack is split into smaller files.

#[cfg(feature = "observe")]
pub(crate) use crate::odata::extract_tenant;
pub use crate::odata::handle_hints;
pub use crate::odata::handle_metadata;
pub use crate::odata::handle_odata_delete;
pub use crate::odata::handle_odata_get;
pub use crate::odata::handle_odata_patch;
pub use crate::odata::handle_odata_post;
pub use crate::odata::handle_odata_put;
pub use crate::odata::handle_service_document;
