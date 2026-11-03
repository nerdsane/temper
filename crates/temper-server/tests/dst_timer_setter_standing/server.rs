//! What outlives a server process, and one process.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::TENANT;
use super::process_clock::ProcessClock;
use super::specs::{CSDL, JOB, PASS, PRINCIPAL_GENERATION};
use super::world::SimTimerRows;

/// What outlives a process: the journal, the timer rows, the clock and the
/// tenant's people and outsiders.
pub(super) struct World {
    pub(super) events: SimEventStore,
    pub(super) rows: Arc<SimTimerRows>,
    pub(super) clock: Arc<ProcessClock>,
    pub(super) people: Mutex<BTreeSet<String>>,
    pub(super) outsiders: BTreeSet<String>,
}

impl World {
    /// The tenant's Cedar: its people may read its records, `Start` a `Job`
    /// and `Hold` it, and its outsiders may only `Start` and `Hold` one, each
    /// themselves or through an agent acting for them. Both, and the app's
    /// service, may create a `Pass`.
    pub(super) fn cedar(&self) -> String {
        let listed = |ids: &BTreeSet<String>| {
            ids.iter()
                .map(|id| format!("\"{id}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let people = self.people.lock().unwrap().clone();
        let creators = listed(&people.union(&self.outsiders).cloned().collect());
        let people = listed(&people);
        let outsiders = listed(&self.outsiders);
        format!(
            r#"permit(principal, action in [Action::"read", Action::"Start", Action::"Hold"], resource)
when {{ (principal is Customer && [{people}].contains(principal.id)) ||
        (context has actingFor && [{people}].contains(context.actingFor)) }};
permit(principal, action in [Action::"Start", Action::"Hold"], resource is Job)
when {{ (principal is Customer && [{outsiders}].contains(principal.id)) ||
        (context has actingFor && [{outsiders}].contains(context.actingFor)) }};
permit(principal, action == Action::"create", resource is Pass)
when {{ (principal is Customer && [{creators}].contains(principal.id)) ||
        (context has actingFor && [{creators}].contains(context.actingFor)) ||
        principal == Agent::"service:app" }};"#
        )
    }
}

/// One server process.
pub(super) struct Process {
    pub(super) runtime: tokio::runtime::Runtime,
    pub(super) state: ServerState,
}

impl Process {
    /// Boot: specs that passed the verification cascade, as a deployed
    /// app's have (the create route refuses a type whose specs have not),
    /// storage, the tenant's Cedar as stored, then `start_runtime`, whose
    /// walk re-arms the timers the last process left.
    pub(super) fn start(world: &World, name: &str) -> Self {
        Self::start_with_job(world, name, JOB)
    }

    /// Boot with a job spec that exercises a recovery path.
    pub(super) fn start_with_job(world: &World, name: &str, job: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                TENANT,
                parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[
                    ("Job", job),
                    ("Pass", PASS),
                    ("PrincipalGeneration", PRINCIPAL_GENERATION),
                ],
            );
            for entity_type in ["Job", "Pass", "PrincipalGeneration"] {
                registry.set_verification_status(
                    &TenantId::new(TENANT),
                    entity_type,
                    VerificationStatus::Completed(EntityVerificationResult {
                        all_passed: true,
                        levels: vec![EntityLevelSummary {
                            level: "L0 SMT".to_string(),
                            passed: true,
                            summary: "OK".to_string(),
                            details: None,
                        }],
                        verified_at: "2026-01-01T00:00:00Z".to_string(),
                    }),
                );
            }
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            state
                .authz
                .reload_tenant_policies(TENANT, &world.cedar())
                .expect("the tenant's Cedar parses");
            let walk = state.start_runtime().await.expect("the server starts");
            walk.await.unwrap();
            state
        });
        Self { runtime, state }
    }

    /// Exit: every task of this process stops.
    pub(super) fn kill(self, world: &World, downtime: Duration) {
        self.runtime.block_on(async { world.clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// Let every task that can run, run.
pub(super) async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next.
pub(super) async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}
