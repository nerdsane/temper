//! Platform invariant checkers (P1–P17) for deterministic simulation testing.
//!
//! Each invariant is a standalone function that inspects the harness state
//! and returns `Ok(())` on success or `Err(message)` on violation.
//! All invariants run against PRODUCTION data structures — no test-only
//! reimplementations.
#![allow(dead_code)]
#![allow(clippy::await_holding_lock)]

use std::collections::{BTreeMap, BTreeSet};

use temper_jit::table::TransitionTable;
use temper_runtime::tenant::{TenantId, parse_persistence_id_parts};
use temper_server::platform_store::PlatformStore;
use temper_server::storage::BoxedEventStore;

use super::platform_harness::SimPlatformHarness;

fn event_store(harness: &SimPlatformHarness) -> Option<BoxedEventStore> {
    harness
        .platform_state
        .server
        .storage_stack
        .as_ref()
        .map(|stack| stack.events.clone())
}

mod persistence;
mod registry;
mod semantics;

// Each integration-test crate compiles this shared module independently and
// intentionally consumes a different subset of the invariant families.
#[allow(unused_imports)]
pub use persistence::*;
#[allow(unused_imports)]
pub use registry::*;
#[allow(unused_imports)]
pub use semantics::*;
