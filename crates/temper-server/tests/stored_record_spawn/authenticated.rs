//! Spawn previews bind trusted inputs with the same caller as live dispatch.
use super::*;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::scheduler::DeterministicRng;
use temper_server::storage::StorageStack;
use temper_store_sim::SimEventStore;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"
strict_action_params = true

[[state]]
name = "owner"
type = "string"
initial = ""

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"
params = [{name = "owner", type = "string", source = "authenticated_subject"}]
[[action.constraints]]
kind = "param_nonempty"
param = "owner"
"#;

const CROSS: &str = r#"
version = 1
[[invariant]]
name = "StartedChildIsInitialized"
kind = "hard"
on = "Run.Start"
assert = 'related(Child, ChildRef).status in ["Initialized"]'
"#;

async fn start_as(
    state: &ServerState,
    id: &str,
    security: SecurityContext,
    params: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(format!("/tdata/Runs('{id}')/Temper.Spawn.Start"))
        .header("Content-Type", "application/json")
        .body(Body::from(params.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            security,
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test(start_paused = true)]
async fn dst_child_preview_binds_the_authenticated_subject_and_preserves_refusals() {
    let mut conflicts = 0;
    for seed in 0..16 {
        let (_guard, _clock, _id_gen) =
            temper_runtime::scheduler::install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let sim = SimEventStore::no_faults(seed);
        let mut state = build_state_with_specs(CHILD, RUN_IOA, CROSS);
        state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
        for (case, acting_for, supplied, accepted) in [
            ("principal", None, None, true),
            ("delegated", Some("alice"), None, true),
            ("forged", Some("alice"), Some("bob"), false),
            ("matching", Some("alice"), Some("alice"), false),
            ("service", None, None, false),
        ] {
            let id = format!("{case}-{seed}");
            // Existing children reach dispatch's authenticated binding before
            // their initializer contract is checked.
            for collection in ["Children", "Runs"] {
                let (status, body) = send(
                    &state,
                    Method::POST,
                    &format!("/tdata/{collection}"),
                    json!({"Id": id}),
                )
                .await;
                assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
            }
            let inject_conflict = accepted && rng.chance(0.5);
            let persistence_id = format!("default:Child:{id}");
            if inject_conflict {
                state
                    .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                    .await
                    .unwrap();
                sim.inject_concurrency_violations(&persistence_id, 1);
                conflicts += 1;
            }
            let mut identity = SecurityContext::from_verified_jwt(
                "gateway",
                PrincipalKind::Agent,
                Some("test"),
                acting_for,
                None,
                None,
            );
            if case == "service" {
                identity.principal.role = Some("service".into());
            }
            let mut params = json!({"child_id": id});
            if let Some(owner) = supplied {
                params["owner"] = json!(owner);
            }
            let (status, body) = start_as(&state, &id, identity, params).await;
            assert_eq!(
                status,
                if accepted {
                    StatusCode::OK
                } else {
                    StatusCode::CONFLICT
                },
                "seed {seed}, {case}: {body}",
            );
            assert_eq!(
                stored_run(&state, &id).await.0,
                if accepted { "Running" } else { "Idle" }
            );
            let child = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let child = state
                        .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                        .await
                        .unwrap();
                    if !accepted || child.state.status == "Initialized" {
                        break child;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("seed {seed}, {case}: initializer did not land"));
            assert_eq!(
                child.state.status,
                if accepted { "Initialized" } else { "New" }
            );
            assert_eq!(
                child.state.fields["owner"],
                if accepted {
                    acting_for.unwrap_or("gateway")
                } else {
                    ""
                }
            );
            if inject_conflict {
                assert_eq!(
                    sim.pending_concurrency_violations(&persistence_id),
                    0,
                    "seed {seed}, {case}: initializer must retry the journal conflict"
                );
            }
            let actions: Vec<_> = sim
                .dump_journal(&persistence_id)
                .into_iter()
                .filter(|event| event.event_type != "Created")
                .map(|event| event.event_type)
                .collect();
            assert_eq!(
                actions,
                if accepted { vec!["Init"] } else { vec![] },
                "seed {seed}, {case}"
            );
        }
    }
    assert!(
        conflicts > 0,
        "the workload must delay authenticated initializers"
    );
}
