//! Seeded simulations of a cross-entity guard reading a record's status while
//! the record changes.
//!
//! A Post may be published only while its Member is Active. Each seed creates
//! one Member and one Post, then runs work on the Member at once with a read
//! of its status (the Post's guard), in one single-threaded execution against
//! the simulated journal. The cooperative yield points (`buggify_yield`)
//! between a commit and its status cache write, and between a status read and
//! its backfill, take a seeded number of turns, so the writes land in a
//! different order on each seed and the same seed replays the same execution.
//!
//! The invariant: once the work is done, the guard decides on the Member's
//! newest committed status, the one its actor holds.

mod common;

use temper_runtime::ActorSystem;
use temper_runtime::buggify::{buggify_yield, install_buggify_context};
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.StatusCache" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Member">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Post">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="member_id" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Members" EntityType="Temper.StatusCache.Member"/>
        <EntitySet Name="Posts" EntityType="Temper.StatusCache.Post"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Touch` commits an event and keeps the Member Active, so its status write
/// says Active while a later `Suspend` may already have committed. It counts
/// itself so that it changes the record: a machine's move that changes
/// nothing writes no event (`entity_actor::unrecorded`).
const MEMBER_IOA: &str = r#"
[automaton]
name = "Member"
states = ["Active", "Suspended"]
initial = "Active"

[[state]]
name = "touches"
type = "counter"
initial = 0

[[action]]
name = "Touch"
from = ["Active"]
to = "Active"
effect = ["touches += 1"]

[[action]]
name = "Suspend"
from = ["Active"]
to = "Suspended"

[[action]]
name = "Reinstate"
from = ["Suspended"]
to = "Active"
"#;

/// `Publish` loops on Draft so the guard can decide again after the race.
const POST_IOA: &str = r#"
[automaton]
name = "Post"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "member_id"
type = "string"
initial = ""

[[action]]
name = "Publish"
from = ["Draft"]
to = "Draft"
guard = "Member[member_id].status in ['Active']"
"#;

const MEMBER: &str = "m-1";
const POST: &str = "p-1";
const MEMBER_ACTIONS: [&str; 3] = ["Touch", "Suspend", "Reinstate"];

/// Seeds explored on every run besides the regression cases.
const EXPLORED_SEEDS: u64 = 64;

/// The first seed of each scenario that broke the invariant before status
/// writes were ordered by the record's event sequence (CI run 40082463981):
/// racing writes left Suspended cached after Reinstate committed Active
/// (seed 3), a delete left Active cached (seed 0), and a write that landed
/// after passivation dropped the newer entry left Active cached (seed 0).
const RACING_WRITES_REGRESSION_SEEDS: &[u64] = &[3];
const DELETE_REGRESSION_SEEDS: &[u64] = &[0];
const PASSIVATION_REGRESSION_SEEDS: &[u64] = &[0];

fn seeds(regressions: &'static [u64]) -> impl Iterator<Item = u64> {
    let explored = (0..EXPLORED_SEEDS).filter(|seed| !regressions.contains(seed));
    regressions.iter().copied().chain(explored)
}

/// The workload's own choices, drawn from the seed (xorshift64), apart from
/// the yields the buggify context draws.
struct Workload(u64);

impl Workload {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        choices[(x % choices.len() as u64) as usize]
    }
}

/// A Member and a Post that names it, on the simulated journal. The Member's
/// status is not cached yet, so the first guard read takes the slow path.
async fn member_and_post(seed: u64, scenario: &str) -> Result<ServerState, String> {
    let csdl = parse_csdl(CSDL).map_err(|e| format!("setup: CSDL: {e:?}"))?;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[("Member", MEMBER_IOA), ("Post", POST_IOA)],
    );
    let mut state = ServerState::from_registry(
        ActorSystem::new(format!("status-cache-{scenario}-{seed}")),
        registry,
    );
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Member", MEMBER, serde_json::json!({}))
        .await
        .map_err(|e| format!("setup: create Member: {e}"))?;
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Post",
            POST,
            serde_json::json!({ "member_id": MEMBER }),
        )
        .await
        .map_err(|e| format!("setup: create Post: {e}"))?;
    Ok(state)
}

/// One Member action; the outcome is kept for the trace.
async fn member_action(state: &ServerState, action: &str) -> String {
    let outcome = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Member",
            MEMBER,
            action,
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    match outcome {
        Ok(r) if r.success => format!(
            "{action} committed {} at sequence {}",
            r.state.status, r.state.sequence_nr
        ),
        Ok(r) => format!("{action} refused: {}", r.error.unwrap_or_default()),
        Err(e) => format!("{action} failed: {e}"),
    }
}

/// Whether the Post's guard lets `Publish` through.
async fn publish_allowed(state: &ServerState) -> bool {
    matches!(
        state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Post",
                POST,
                "Publish",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await,
        Ok(r) if r.success
    )
}

/// A guard read racing the work: the Post's guard resolves the Member's
/// status, from the cache or from its actor with a backfill.
async fn guard_read(state: &ServerState) -> String {
    format!(
        "guard read allowed Publish: {}",
        publish_allowed(state).await
    )
}

/// Delete the Member through the kernel's delete.
async fn delete_member(state: &ServerState) -> String {
    match state
        .delete_tenant_entity(&TenantId::default(), "Member", MEMBER)
        .await
    {
        Ok(r) => format!(
            "delete committed {} at sequence {}",
            r.state.status, r.state.sequence_nr
        ),
        Err(e) => format!("delete failed: {e}"),
    }
}

/// Passivate the Member's actor once it has been idle past the timeout.
async fn passivate_member(state: &ServerState) -> String {
    buggify_yield(8).await;
    let actor_key = format!("{}:Member:{MEMBER}", TenantId::default());
    if let Ok(mut last_accessed) = state.last_accessed.write() {
        last_accessed.insert(actor_key, sim_now() - chrono::Duration::seconds(600));
    }
    state.passivate_idle_actors().await;
    "passivated the Member".to_string()
}

/// The invariant, checked once the racing work is done: the guard allows
/// `Publish` exactly when the Member's newest committed status, read from its
/// actor, is Active. `Err` names the violation with the trace.
async fn guard_decides_on_newest_status(
    state: &ServerState,
    trace: &[String],
) -> Result<(), String> {
    let newest = state
        .get_tenant_entity_state(&TenantId::default(), "Member", MEMBER)
        .await
        .map(|r| (r.state.status, r.state.sequence_nr));
    let should_allow = matches!(&newest, Ok((status, _)) if status == "Active");
    let cached = state
        .resolve_entity_status(&TenantId::default(), "Member", MEMBER)
        .await;
    let allowed = publish_allowed(state).await;
    if allowed == should_allow {
        return Ok(());
    }
    Err(format!(
        "the guard {} Publish but the Member's newest committed status is {newest:?} \
         (status read by the guard: {cached:?}); trace: {}",
        if allowed { "allowed" } else { "refused" },
        trace.join("; ")
    ))
}

/// Three Member actions drawn from the seed run at once with a guard read
/// whose cache miss reads the actor and backfills.
async fn racing_status_writes(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = member_and_post(seed, "writes").await?;
    let mut workload = Workload::new(seed);
    let (a, b, c) = (
        workload.pick(&MEMBER_ACTIONS),
        workload.pick(&MEMBER_ACTIONS),
        workload.pick(&MEMBER_ACTIONS),
    );
    let _faults = install_buggify_context(seed);
    let (read, a, b, c) = tokio::join!(
        guard_read(&state),
        member_action(&state, a),
        member_action(&state, b),
        member_action(&state, c),
    );
    guard_decides_on_newest_status(&state, &[read, a, b, c]).await
}

/// A status write and a guard read race the Member's delete.
async fn delete_racing_status_writes(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = member_and_post(seed, "delete").await?;
    let _faults = install_buggify_context(seed);
    let (touch, read, deleted) = tokio::join!(
        member_action(&state, "Touch"),
        guard_read(&state),
        delete_member(&state),
    );
    guard_decides_on_newest_status(&state, &[touch, read, deleted]).await
}

/// Two status writes race the passivation of the Member's actor.
async fn passivation_racing_status_writes(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let state = member_and_post(seed, "passivation").await?;
    let _faults = install_buggify_context(seed);
    let (touch, suspend, passivated) = tokio::join!(
        member_action(&state, "Touch"),
        member_action(&state, "Suspend"),
        passivate_member(&state),
    );
    guard_decides_on_newest_status(&state, &[touch, suspend, passivated]).await
}

#[tokio::test]
async fn dst_status_cache_guard_decides_on_the_newest_of_racing_writes() {
    for seed in seeds(RACING_WRITES_REGRESSION_SEEDS) {
        if let Err(violation) = racing_status_writes(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_status_cache_guard_does_not_trust_a_deleted_record() {
    for seed in seeds(DELETE_REGRESSION_SEEDS) {
        if let Err(violation) = delete_racing_status_writes(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn dst_status_cache_guard_decides_on_the_newest_across_passivation() {
    for seed in seeds(PASSIVATION_REGRESSION_SEEDS) {
        if let Err(violation) = passivation_racing_status_writes(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}
