//! temper-authz: Cedar-based authorization for Temper.
//!
//! Integrates AWS Cedar policy language for fine-grained, entity-aware
//! authorization decisions on OData operations. Every actor message dispatch
//! goes through: Request → SecurityContext → Cedar Evaluate → Allow/Deny.

mod context;
mod engine;
mod error;

pub use context::{SecurityContext, Principal, PrincipalKind};
pub use engine::{AuthzEngine, AuthzDecision};
pub use error::AuthzError;
