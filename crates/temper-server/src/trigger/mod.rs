//! Cross-entity coordination via reaction rules.
//!
//! Reaction rules are platform-level configuration that enable choreography
//! between independent entity state machines. Each rule says: "when entity X
//! completes action A reaching state S, dispatch action B on entity Y."
//!
//! This preserves IOA spec purity (single-entity, deterministic, independently
//! verifiable) while enabling multi-entity workflows like e-commerce order
//! fulfilment cascades.

// ADR-0181 requires this foundation to remain inert until the final activation gate.
#[allow(dead_code, unused_imports)]
pub mod collection_workflow;
pub mod delivery;
pub mod dispatcher;
pub(crate) mod guard;
pub(crate) mod params;
pub mod registry;
pub(crate) mod resolver;
pub mod sim_dispatcher;
pub mod types;

pub use dispatcher::ReactionDispatcher;
pub use registry::{ReactionRegistry, parse_reactions};
pub use sim_dispatcher::SimReactionSystem;
pub use types::{
    MAX_GUARD_DEPTH, MAX_REACTION_DEPTH, MAX_REACTIONS_PER_TENANT, ReactionFailureKind,
    ReactionGuard, ReactionResult, ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver,
};
