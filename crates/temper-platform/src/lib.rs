//! temper-platform: Dogfooded hosting platform for Temper.
//!
//! Provides the platform infrastructure:
//! - **Verify-and-deploy pipeline**: Accepts pre-authored IOA TOML + CSDL specs,
//!   runs the verification cascade, and registers tenants with hot-deployed actors.
//! - **OData API**: All entities (system and user) are accessible via the
//!   Temper Data API (`/tdata`), following OData v4 standard.

pub mod bearer_auth;
pub mod bootstrap;
pub mod deploy;
pub mod genesis_install;
mod genesis_install_verify;
pub mod hooks;
pub mod integration;
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
    IssuerBootstrapOutcome, bootstrap_agent_specs, bootstrap_identity_specs,
    bootstrap_operator_credential, bootstrap_operator_credential_specs, bootstrap_system_tenant,
    bootstrap_trusted_issuer_from_env, persist_agent_verification, persist_system_verification,
};
pub use os_apps::{AppBundle, AppEntry, AppManifest, InstallResult, install_os_app, list_os_apps};
pub use protocol::{PlatformEvent, VerifyStepStatus};
pub use state::PlatformState;
