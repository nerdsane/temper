//! Who acted, which instance, what kind and for whom are kernel facts: history
//! and a record read say the same, whichever way the move came about.
//!
//! Every call goes through the platform router with a real ES256 token, so the
//! bearer boundary, the resolver and the record reads all run as in production.

#[path = "agent_label/binding.rs"]
mod binding;
#[path = "agent_label/fixture.rs"]
mod fixture;
#[path = "agent_label/propagation.rs"]
mod propagation;
#[path = "agent_label/reads.rs"]
mod reads;
