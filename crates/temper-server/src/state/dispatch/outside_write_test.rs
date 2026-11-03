//! DST: two writes from outside the kernel race on one record.
//!
//! `Panel` carries one field invariant, "only one side is on". Turning the
//! left side on and turning the right side on are each valid alone; together
//! they break it. Each seed starts the two writes in a seeded order with
//! seeded delays and polls both on this one thread (no spawned tasks), so some
//! seeds read the record before either write lands and others run one after
//! the other.
//!
//! Invariant: no write commits against a version it did not read. Exactly one
//! write commits, the stored record keeps the invariant, and the other write
//! is refused: as stale with the record's current version when it read the
//! record before the first one landed, or by the invariant when it read the
//! record after.

use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

use crate::entity_actor::EntityResponse;
use crate::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use crate::request_context::AgentContext;
use crate::state::{DispatchCommand, DispatchError, ServerState, StaleWrite};

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Race" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Panel">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Left" Type="Edm.String"/>
        <Property Name="Right" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="RaceService">
        <EntitySet Name="Panels" EntityType="Temper.Race.Panel"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const PANEL_IOA: &str = r#"
[automaton]
name = "Panel"
states = ["Live"]
initial = "Live"

[[action]]
name = "SetLeft"
kind = "input"
from = ["Live"]
params = ["Left"]

[[action]]
name = "SetRight"
kind = "input"
from = ["Live"]
params = ["Right"]

[[field_invariant]]
name = "OneSideOn"
message = "only one side of a panel is on"
assert = "Left == 'on' => Right == null"
"#;

const SEEDS: u64 = 64;

fn build_state(seed: u64) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Panel", PANEL_IOA)],
        None,
    );
    let state = ServerState::from_registry(ActorSystem::new(format!("race-{seed}")), registry);
    state
        .registry
        .write()
        .expect("registry lock")
        .set_verification_status(
            &TenantId::default(),
            "Panel",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-10-24T00:00:00Z".to_string(),
            }),
        );
    state
}

/// One outside write, started after `delay` yields of this thread.
async fn outside_write(
    state: &ServerState,
    delay: usize,
    action: &str,
    params: Value,
) -> Result<EntityResponse, DispatchError> {
    for _ in 0..delay {
        tokio::task::yield_now().await;
    }
    let agent_ctx = AgentContext::default();
    state
        .dispatch_outside_action(DispatchCommand {
            tenant: &TenantId::default(),
            entity_type: "Panel",
            entity_id: "p1",
            action,
            params,
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
}

#[tokio::test]
async fn racing_outside_writes_commit_only_against_the_version_they_read() {
    let mut stale_seeds = 0u64;
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let state = build_state(seed);
        let tenant = TenantId::default();
        state
            .get_or_create_tenant_entity(&tenant, "Panel", "p1", json!({}))
            .await
            .expect("panel is created");

        let left_delay = rng.next_bound(4);
        let right_delay = rng.next_bound(4);
        let left = outside_write(&state, left_delay, "SetLeft", json!({"Left": "on"}));
        let right = outside_write(&state, right_delay, "SetRight", json!({"Right": "on"}));
        let (left, right) = if rng.chance(0.5) {
            tokio::join!(left, right)
        } else {
            let (right, left) = tokio::join!(right, left);
            (left, right)
        };

        let stored = state
            .get_tenant_entity_state(&tenant, "Panel", "p1")
            .await
            .expect("panel is readable")
            .state;
        let trace = format!(
            "seed {seed} (delays left {left_delay}, right {right_delay}): \
             left {left:?}, right {right:?}, stored {} at version {}",
            stored.fields, stored.sequence_nr
        );
        let committed = [&left, &right]
            .iter()
            .filter(|outcome| matches!(outcome, Ok(response) if response.success))
            .count();
        assert_eq!(committed, 1, "exactly one racing write commits; {trace}");
        assert!(
            !(stored.fields.get("Left") == Some(&json!("on"))
                && stored.fields.get("Right") == Some(&json!("on"))),
            "the stored record keeps OneSideOn; {trace}"
        );
        for outcome in [&left, &right] {
            match outcome {
                Ok(response) if response.success => {}
                Err(DispatchError::StaleWrite(StaleWrite {
                    entity_type,
                    entity_id,
                    current_version,
                })) => {
                    assert_eq!(
                        (entity_type.as_str(), entity_id.as_str()),
                        ("Panel", "p1"),
                        "{trace}"
                    );
                    assert_eq!(
                        *current_version, stored.sequence_nr,
                        "a stale refusal carries the current version; {trace}"
                    );
                    stale_seeds += 1;
                }
                Ok(refused) => assert!(
                    refused
                        .error
                        .as_deref()
                        .is_some_and(|error| error.contains("only one side of a panel is on")),
                    "a write that read the committed record is refused by the invariant; {trace}"
                ),
                Err(other) => panic!("unexpected dispatch error {other}; {trace}"),
            }
        }
    }
    assert!(
        stale_seeds > 0,
        "no seed raced both reads ahead of the first write; the workload does not exercise the race"
    );
}

#[test]
fn a_failed_reply_on_a_newer_state_is_stale_only_when_the_actor_says_nothing_landed() {
    use super::stale_refusal;
    use crate::entity_actor::EntityActor;
    use crate::entity_actor::effects::entity_authorization_precondition;
    use crate::state::write_record::WriteCaller;

    let table = temper_jit::table::TransitionTable::from_ioa_source(PANEL_IOA);
    let mut current = EntityActor::build_initial_state("Panel", "p1", &table, &json!({}));
    let read = entity_authorization_precondition(&current);
    current.sequence_nr += 1;
    current.fields["Left"] = json!("on");
    let failed = EntityResponse::write_failed(current.clone(), "reply lost".into());
    assert!(stale_refusal(WriteCaller::Outside, Some(&read), "Panel", "p1", &failed).is_none());

    let changed = EntityResponse::record_changed(current, "record changed".into());
    assert!(matches!(
        stale_refusal(WriteCaller::Outside, Some(&read), "Panel", "p1", &changed),
        Some(DispatchError::StaleWrite(StaleWrite {
            current_version: 1,
            ..
        }))
    ));
    assert!(stale_refusal(WriteCaller::Kernel, Some(&read), "Panel", "p1", &changed).is_none());
}
