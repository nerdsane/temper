//! OTS data models
//!
//! Core types for the Open Trajectory Specification, adapted for Temper's
//! deterministic simulation requirements.

pub mod annotation;
pub mod context;
pub mod decision;
pub mod enums;
pub mod message;
pub mod trajectory;
pub mod turn;

// Re-export commonly used types
pub use annotation::*;
pub use context::*;
pub use decision::*;
pub use enums::*;
pub use message::*;
pub use trajectory::*;
pub use turn::*;
