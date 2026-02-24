//! temper-platform: Dogfooded hosting platform for Temper.
//!
//! Provides the platform infrastructure:
//! - **Verify-and-deploy pipeline**: Accepts pre-authored IOA TOML + CSDL specs,
//!   runs the verification cascade, and registers tenants with hot-deployed actors.
//! - **Evolution Engine**: Captures unmet intents from production usage, creates
//!   O-Records and I-Records, and routes approval requests to developers.
//! - **Agentic Evolution**: Claude-powered agents formalize observations into
//!   problem statements (O→P) and propose spec changes (P→A).
//! - **OData API**: All entities (system and user) are accessible via the
//!   Temper Data API (`/tdata`), following OData v4 standard.

pub mod agent;
pub mod bootstrap;
pub mod deploy;
pub mod spec_store;
pub mod evolution;
pub mod hooks;
pub mod integration;
pub mod optimization;
pub mod protocol;
pub mod router;
pub mod state;

// Re-export primary types at crate root.
pub use bootstrap::bootstrap_system_tenant;
pub use protocol::{PlatformEvent, VerifyStepStatus};
pub use state::PlatformState;
