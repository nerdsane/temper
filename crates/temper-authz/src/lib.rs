//! temper-authz: Cedar-based authorization for Temper.
//!
//! Integrates AWS Cedar policy language for fine-grained, entity-aware
//! authorization decisions on OData operations. Every actor message dispatch
//! goes through: Request → SecurityContext → Cedar Evaluate → Allow/Deny.

mod cause;
mod context;
mod engine;
mod error;
mod metrics;
mod policy_gen;
pub mod statements;

pub use cause::{MoveCause, ReactionHop, TimerArm, TimerRecord};
pub use context::{
    AuthenticatedRequestContext, IssuedCredential, Principal, PrincipalKind, SecurityContext,
    is_cedar_authority_context_key, platform_keys,
};
pub use engine::{
    Answered, Asker, Attr, AuthzDecision, AuthzEngine, Cause, Hop, Kind, ListRead,
    ListReadUniformity, May, Question, Read, RowRule, SendAnalysis, VERIFIED_OPERATOR_WHEN,
    permitted_resource_types,
};
pub use error::{AuthzDenial, AuthzError};
pub use metrics::init_metrics;
pub use policy_gen::{
    ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope,
    generate_cedar_from_matrix, validate_policy_scope_matrix,
};
