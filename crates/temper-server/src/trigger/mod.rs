//! Cross-entity coordination via reaction rules.
//!
//! Reaction rules are platform-level configuration that enable choreography
//! between independent entity state machines. Each rule says: "when entity X
//! completes action A reaching state S, dispatch action B on entity Y."
//!
//! This preserves IOA spec purity (single-entity, deterministic, independently
//! verifiable) while enabling multi-entity workflows like e-commerce order
//! fulfilment cascades.

pub mod dispatcher;
pub(crate) mod guard;
mod owed_reactions;
pub(crate) mod params;
mod principal;
pub mod registry;
mod repeat_stop;
pub(crate) mod resolver;
pub mod sim_dispatcher;
pub mod types;
mod withheld;

pub use dispatcher::ReactionDispatcher;
pub use registry::ReactionRegistry;
pub use sim_dispatcher::SimReactionSystem;
pub use types::{
    MAX_REACTIONS_PER_TENANT, ReactionResult, ReactionRule, ReactionTarget, ReactionTrigger,
    TargetResolver,
};
