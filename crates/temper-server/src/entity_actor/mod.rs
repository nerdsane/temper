//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.

mod actor;
pub mod types;

pub use actor::EntityActor;
pub use types::{EntityEvent, EntityMsg, EntityResponse, EntityState};
