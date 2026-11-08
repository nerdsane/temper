//! Related records are read through their configured runtime, independently
//! of the runtime that will store the action's result.
use super::*;
use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};

const RELATED_CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Target">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="TargetId" Type="Edm.String"/>
        <NavigationProperty Name="Target" Type="Temper.Target">
          <ReferentialConstraint Property="TargetId" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="Service">
        <EntitySet Name="Orders" EntityType="Temper.Order"/>
        <EntitySet Name="Targets" EntityType="Temper.Target"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
[[state]]
name = "TargetId"
type = "string"
initial = ""
[[action]]
name = "Submit"
kind = "input"
from = ["Draft"]
to = "Submitted"
params = ["TargetId"]
"#;

const TARGET_SPEC: &str = r#"
[automaton]
name = "Target"
states = ["Active", "Archived"]
initial = "Active"
[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Archived"
"#;

#[tokio::test]
async fn relation_targets_use_their_own_runtime() {
    check_targets(true, None).await;
}

#[tokio::test]
async fn cross_invariant_status_uses_the_targets_runtime() {
    check_targets(false, Some(("status", "Active"))).await;
}

#[tokio::test]
async fn cross_invariant_field_uses_the_targets_runtime() {
    check_targets(false, Some(("Eligibility", "allowed"))).await;
}

async fn check_targets(relation: bool, invariant: Option<(&str, &str)>) {
    let seed = match invariant {
        None => 86,
        Some(("status", _)) => 87,
        Some(_) => 88,
    };
    let _sim = install_deterministic_context(seed);
    let (pool, _container) = pool().await;
    let actors = Arc::new(ActorSystem::new(pool.clone(), SchedulerConfig::default()));
    for spec in [ORDER_SPEC, TARGET_SPEC] {
        actors
            .register(Arc::new(SpecDrivenActor::from_ioa(spec).unwrap()))
            .await
            .unwrap();
    }
    // Test both directions across runtimes, as well as all-Postgres actions.
    for (source_pg, target_pg) in [(true, true), (true, false), (false, true)] {
        let state = make_state(actors.clone(), (source_pg, target_pg), relation, invariant);
        let router = build_router(state.clone()).layer(axum::middleware::from_fn(test_identity));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = Client::new();
        let id = sim_uuid().to_string();
        let target_id = sim_uuid().to_string();
        let handle = ActorHandle::new(format!("default/{id}"), "Order");
        spawn_record(&state, &actors, "Order", &id, source_pg, json!({})).await;
        spawn_record(
            &state,
            &actors,
            "Target",
            &target_id,
            target_pg,
            json!({"Eligibility":"allowed"}),
        )
        .await;
        // A conflicting native copy must not stand in for the configured runtime.
        if target_pg && invariant.is_some() {
            spawn_record(
                &state,
                &actors,
                "Target",
                &target_id,
                false,
                json!({"Eligibility":"denied"}),
            )
            .await;
            archive_target(&state, &actors, &target_id, false).await;
        }
        let initial = if source_pg {
            Some(actor_state(&pool, &handle).await)
        } else {
            None
        };
        let native_initial = if source_pg {
            None
        } else {
            Some(
                serde_json::to_value(
                    state
                        .get_tenant_entity_state(&TenantId::default(), "Order", &id)
                        .await
                        .unwrap()
                        .state,
                )
                .unwrap(),
            )
        };
        let action_url = format!("{base}/tdata/Orders('{id}')/Temper.Submit");
        // Missing targets are refused before anything is queued or changed.
        let missing = sim_uuid().to_string();
        // A record in another tenant must not satisfy the reference.
        actors
            .spawn_with_fields(
                &format!("other/{missing}"),
                "Target",
                json!({"Eligibility":"allowed"}),
            )
            .await
            .unwrap();
        let request = PendingAction {
            state: &state,
            actors: &actors,
            pool: &pool,
            handle: &handle,
            initial: initial.as_deref(),
            native_initial: native_initial.as_ref(),
            client: &client,
            url: &action_url,
        };
        request.reject(&missing, None).await;
        // A present record may still fail a hard invariant. Test that separately
        // from relation integrity so the existence check cannot hide a bad read.
        if let Some((field, _)) = invariant {
            request.reject_invalid(target_pg, field).await;
        }
        request.accept(&target_id).await;
        server.abort();
    }
}

fn make_state(
    actors: Arc<ActorSystem>,
    (source_pg, target_pg): (bool, bool),
    relation: bool,
    invariant: Option<(&str, &str)>,
) -> ServerState {
    let csdl = if relation {
        RELATED_CSDL.to_string()
    } else {
        RELATED_CSDL.replace(
            r#"<ReferentialConstraint Property="TargetId" ReferencedProperty="Id"/>"#,
            "",
        )
    };
    let cross = invariant.map(|(field, value)| {
        format!(
            r#"
version = 1
[[invariant]]
name = "EligibleTarget"
kind = "hard"
on = "Order.Submit"
assert = 'related(Target, TargetId).{field} in ["{value}"]'
"#
        )
    });
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        temper_spec::csdl::parse_csdl(&csdl).unwrap(),
        csdl,
        &[("Order", ORDER_SPEC), ("Target", TARGET_SPEC)],
        cross,
    );
    for entity_type in ["Order", "Target"] {
        registry.set_verification_status(
            &TenantId::default(),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-10-30T00:00:00Z".into(),
            }),
        );
    }
    let mut state = ServerState::from_pg_registry(actors.clone(), registry);
    // Use tenant-qualified configuration to cover the per-tenant runtime choice.
    for (name, pg) in [("Order", source_pg), ("Target", target_pg)] {
        if pg {
            state.actor_backed_types.insert(format!("default:{name}"));
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state
}

async fn spawn_record(
    state: &ServerState,
    actors: &ActorSystem,
    entity_type: &str,
    id: &str,
    pg: bool,
    fields: Value,
) {
    if pg {
        actors
            .spawn_with_fields(&format!("default/{id}"), entity_type, fields)
            .await
            .unwrap();
    } else {
        state
            .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, fields)
            .await
            .unwrap();
    }
}

async fn archive_target(state: &ServerState, actors: &ActorSystem, id: &str, pg: bool) {
    if pg {
        let handle = ActorHandle::new(format!("default/{id}"), "Target");
        actors
            .tell(
                None,
                &handle,
                temper_actor_runtime::spec_actor::SpecMessage::new("Archive"),
            )
            .await
            .unwrap();
        actors.activate_now(&handle).await.unwrap();
    } else {
        state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Target",
                id,
                "Archive",
                json!({}),
                &temper_server::request_context::AgentContext::for_service("test"),
            )
            .await
            .unwrap();
    }
}

/// An action held before dispatch, so refusals can be checked against both
/// its source record and its Postgres mailbox.
struct PendingAction<'a> {
    state: &'a ServerState,
    actors: &'a ActorSystem,
    pool: &'a Pool,
    handle: &'a ActorHandle,
    initial: Option<&'a [u8]>,
    native_initial: Option<&'a Value>,
    client: &'a Client,
    url: &'a str,
}

impl PendingAction<'_> {
    async fn post(&self, target_id: &str) -> reqwest::Response {
        self.client
            .post(self.url)
            .json(&json!({"TargetId":target_id}))
            .send()
            .await
            .unwrap()
    }

    async fn unchanged(&self) {
        if let Some(initial) = self.initial {
            assert_eq!(actor_state(self.pool, self.handle).await, initial);
        } else {
            let id = self.handle.namespace.strip_prefix("default/").unwrap();
            let source = self
                .state
                .get_tenant_entity_state(&TenantId::default(), "Order", id)
                .await
                .unwrap();
            assert_eq!(
                &serde_json::to_value(source.state).unwrap(),
                self.native_initial.unwrap()
            );
        }
    }

    async fn reject(&self, target_id: &str, bad_value: Option<&str>) {
        let response = self.post(target_id).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["error"]["code"],
            if self.initial.is_some() {
                "ConstraintViolation"
            } else {
                "ActionFailed"
            }
        );
        if let Some(value) = bad_value {
            if self.initial.is_some() {
                assert_eq!(body["error"]["details"]["type"], "cross_invariant");
                assert_eq!(body["error"]["details"]["invariant"], "EligibleTarget");
            }
            assert!(
                body["error"]["message"].as_str().unwrap().contains(value),
                "{body}"
            );
        }
        if self.initial.is_some() {
            assert_eq!(message_count(self.pool, &self.handle.namespace).await, 0);
        }
        self.unchanged().await;
    }

    async fn reject_invalid(&self, target_pg: bool, field: &str) {
        let id = sim_uuid().to_string();
        spawn_record(
            self.state,
            self.actors,
            "Target",
            &id,
            target_pg,
            json!({"Eligibility":"denied"}),
        )
        .await;
        archive_target(self.state, self.actors, &id, target_pg).await;
        self.reject(
            &id,
            Some(if field == "status" {
                "Archived"
            } else {
                "denied"
            }),
        )
        .await;
    }

    async fn accept(&self, target_id: &str) {
        let response = self.post(target_id).await;
        assert_eq!(
            response.status(),
            if self.initial.is_some() {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            },
            "{}",
            response.text().await.unwrap()
        );
        if self.initial.is_some() {
            assert_eq!(message_count(self.pool, &self.handle.namespace).await, 1);
            self.unchanged().await;
            self.actors.activate_now(self.handle).await.unwrap();
            let after: Value =
                serde_json::from_slice(&actor_state(self.pool, self.handle).await).unwrap();
            assert_eq!(after["status"], "Submitted");
            assert_eq!(after["fields"]["TargetId"], target_id);
        } else {
            let id = self.handle.namespace.strip_prefix("default/").unwrap();
            let after = self
                .state
                .get_tenant_entity_state(&TenantId::default(), "Order", id)
                .await
                .unwrap();
            assert_eq!(after.state.status, "Submitted");
            assert_eq!(after.state.fields["TargetId"], target_id);
        }
    }
}
