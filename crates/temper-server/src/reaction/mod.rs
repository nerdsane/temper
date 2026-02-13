//! Cross-entity coordination via reaction rules.
//!
//! Reaction rules are platform-level configuration that enable choreography
//! between independent entity state machines. Each rule says: "when entity X
//! completes action A reaching state S, dispatch action B on entity Y."
//!
//! This preserves IOA spec purity (single-entity, deterministic, independently
//! verifiable) while enabling multi-entity workflows like e-commerce order
//! fulfilment cascades.

pub mod types;
pub mod registry;
pub mod sim_dispatcher;
pub mod dispatcher;

pub use types::{
    ReactionRule, ReactionTrigger, ReactionTarget, TargetResolver, ReactionResult,
    MAX_REACTIONS_PER_TENANT, MAX_REACTION_DEPTH,
};
pub use registry::ReactionRegistry;
pub use sim_dispatcher::SimReactionSystem;
pub use dispatcher::ReactionDispatcher;
