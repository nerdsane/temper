//! A timer fires as whoever its record's moves name, and a timer nothing names
//! a caller for stops and says so.
//!
//! A reaction carries its person in memory, but a record made and moved only
//! by reactions kept no one in its journal, so once its timer row was gone
//! nothing could name who its timer fires as: it waited forever, or fired as
//! the state-timer service, whose move binding its caller was refused on every
//! fire, unseen (the Arni Labs line's Rollout, 31 Oct 2026).
//!
//! The fixtures (specs, a server over a store that survives it, reads) are in
//! `timer_needs_person/`; the tests are grouped by what they check.

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, SimContextGuard, install_sim_context,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
use process_clock::ProcessClock;

#[path = "timer_needs_person/specs.rs"]
mod specs;
use specs::*;

#[path = "timer_needs_person/fixture.rs"]
mod fixture;
use fixture::*;

#[path = "timer_needs_person/chains.rs"]
mod chains;

#[path = "timer_needs_person/stops.rs"]
mod stops;

#[path = "timer_needs_person/race.rs"]
mod race;
