//! Optional Postgres runtime adapter (`--actor-runtime postgres`). Not default serve.
//!
//! Mailbox + scheduler in PG. `SpecDrivenActor` evaluates `temper-jit` tables
//! then applies effects here — a second interpreter. Unimplemented `Effect`
//! variants fail closed. Default path is `temper-server` EntityActor.

#[cfg(feature = "test-shared-pg-proofs")]
extern crate self as temper_actor_runtime;

/// Unchanged PostgreSQL integration proofs shared by the workspace test harness.
#[cfg(feature = "test-shared-pg-proofs")]
#[path = "../tests/integration.rs"]
pub mod integration_proofs;

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
pub use spec_actor::{SpecActorState, SpecDrivenActor, SpecMessage, build_actor_routing};
pub use system::ActorSystem;
