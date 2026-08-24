//! temper-platform: Dogfooded hosting platform for Temper.
//!
//! Provides the platform infrastructure:
//! - **Verify-and-deploy pipeline**: Accepts pre-authored IOA TOML + CSDL specs,
//!   runs the verification cascade, and registers tenants with hot-deployed actors.
//! - **OData API**: All entities (system and user) are accessible via the
//!   Temper Data API (`/tdata`), following OData v4 standard.

pub mod app_bundles;
pub mod bearer_auth;
pub mod bootstrap;
pub mod deploy;
pub mod genesis_install;
pub mod hooks;
pub mod integration;
pub mod module_sdk_build;
mod operator_manage_policies;
pub mod optimization;
pub mod os_apps;
pub mod protocol;
pub mod recovery;
pub mod router;
pub mod spec_store;
pub mod state;
pub mod tenant_access;
pub mod tenant_api;

// Re-export primary types at crate root.
pub use bootstrap::{
    bootstrap_agent_specs, bootstrap_operator_credential, bootstrap_system_tenant,
    persist_agent_verification, persist_system_verification,
};
pub use os_apps::{AppBundle, AppEntry, AppManifest, InstallResult, install_os_app, list_os_apps};
pub use protocol::{PlatformEvent, VerifyStepStatus};
pub use state::PlatformState;
