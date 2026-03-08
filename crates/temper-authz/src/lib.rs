//! temper-authz: Cedar-based authorization for Temper.
//!
//! Integrates AWS Cedar policy language for fine-grained, entity-aware
//! authorization decisions on OData operations. Every actor message dispatch
//! goes through: Request → SecurityContext → Cedar Evaluate → Allow/Deny.

mod context;
mod engine;
mod error;

pub use context::{Principal, PrincipalKind, SecurityContext};
pub use engine::{AuthzDecision, AuthzEngine};
pub use error::{AuthzDenial, AuthzError};
