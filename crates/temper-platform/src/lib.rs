//! temper-platform: Conversational development platform.
//!
//! Provides the full developer experience for Temper:
//! - **Developer Chat**: Interview agent that guides developers through entity design,
//!   generates IOA specs + CSDL + Cedar, runs verification cascade, and hot-deploys.
//! - **Production Chat**: Operates within deployed specs, captures unmet intents,
//!   feeds the Evolution Engine for developer approval.
//! - **Web UI**: Replit-like split-pane interface (dev) and clean chat (prod).
//! - **Evolution Pipeline**: Unmet intent → O-Record → I-Record → approval → spec change.

pub mod protocol;
pub mod state;
pub mod interview;
pub mod agent;
pub mod deploy;
pub mod evolution;
pub mod ws;
pub mod router;

// Re-export primary types at crate root.
pub use protocol::{WsMessage, SpecType, VerifyStepStatus};
pub use state::{PlatformState, PlatformMode};
