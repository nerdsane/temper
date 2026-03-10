//! temper-authz: Cedar-based authorization for Temper.
//!
//! Integrates AWS Cedar policy language for fine-grained, entity-aware
//! authorization decisions on OData operations. Every actor message dispatch
//! goes through: Request → SecurityContext → Cedar Evaluate → Allow/Deny.

mod context;
mod engine;
mod error;
mod policy_gen;

pub use context::{Principal, PrincipalKind, SecurityContext};
pub use engine::{AuthzDecision, AuthzEngine};
pub use error::{AuthzDenial, AuthzError};
pub use policy_gen::{
    generate_cedar_from_matrix, ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope,
    ResourceScope,
};
