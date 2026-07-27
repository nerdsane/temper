//! PG-backed mailbox actor runtime for Temper.
//!
//! Actors communicate via addressed messages (mailboxes), not pub/sub.
//! Each actor has a handle `(session_id, actor_type)`, receives messages
//! one at a time (FIFO), and sends messages to other actors via `tell()`
//! (buffered, transactional) or `ask()` (immediate, request-response).
//!
//! This is Layer 1 — the generic runtime. Layer 2 (IOA specs, TransitionTable)
//! builds on top of this.

pub mod actor;
pub mod bus;
pub mod mailbox;
pub mod pg;
pub mod scheduler;
pub mod schema;
pub mod spec_actor;
pub mod system;
#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub use actor::{Actor, ActorContext, ActorError, ActorHandle, Message};
pub use bus::{BUS_ACTOR_TYPE, CallMsg, CallReply, StreamMsg};
pub use mailbox::{Mailbox, MailboxError};
pub use pg::{ActivationError, ActivationResult, PgActorActivator, PgMailbox, PgMailboxConfig};
pub use scheduler::{Scheduler, SchedulerConfig};
pub use spec_actor::{
    SpecActorState, SpecDrivenActor, SpecMessage, build_actor_routing, build_routing_maps,
};
pub use system::{ActorSpawnOutcome, ActorSystem};
