//! An action's answer is the record as a read shows it, without its recent events. Each event keeps its params in
//! full, so fifty moves that each carry a large text made the answer tens of megabytes: an HttpEndpoint admission
//! reads that answer within 8 MiB, and refused the route with a 500 (the Factory's repository route, 30 Oct 2026).

use axum::{
    Extension,
    body::{Bytes, to_bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState,
    registry::{EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus},
};

const IOA: &str = r#"
[automaton]
name = "Draft"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]
strict_action_params = true
[[state]]
name = "said"
type = "string"
initial = ""
[[action]]
name = "Write"
from = ["Open"]
params = [{ name = "text", type = "string" }]
[[action]]
name = "Say"
from = ["Open"]
params = [{ name = "said", type = "string" }]
"#;
const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Draft"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="said" Type="Edm.String"/>
</EntityType>
<Action Name="Write" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Draft"/></Action>
<Action Name="Say" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Draft"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Drafts" EntityType="Probe.Draft"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// What an endpoint admission holds of one answer (http_endpoint/admission.rs).
const ADMISSION_ANSWER_BUDGET: usize = 8 * 1024 * 1024;

fn fixture() -> ServerState {
    let verified = temper_verify::cascade::VerificationCascade::from_ioa(IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(verified.all_passed, "probe IOA verification failed");
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            "answers",
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Draft", IOA)],
        )
        .unwrap();
    registry.set_verification_status(
        &TenantId::new("answers"),
        "Draft",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: verified.all_passed,
            levels: verified
                .levels
                .into_iter()
                .map(|r| EntityLevelSummary {
                    level: r.level.to_string(),
                    passed: r.passed,
                    summary: r.summary,
                    details: None,
                })
                .collect(),
            verified_at: "2026-10-30T00:00:00Z".into(),
        }),
    );
    let state = ServerState::from_registry(ActorSystem::new("action-answer-events"), registry);
    state
        .authz
        .reload_tenant_policies("answers", "permit(principal, action, resource is Draft);")
        .unwrap();
    state
}

async fn post(
    state: &ServerState,
    path: &str,
    params: Value,
    idempotency_key: Option<&str>,
) -> (StatusCode, Bytes) {
    let identity = SecurityContext::from_verified_jwt(
        "action-answer-events",
        PrincipalKind::Agent,
        Some("writer"),
        Some("alice"),
        None,
        None,
    );
    let mut headers = HeaderMap::new();
    if let Some(key) = idempotency_key {
        headers.insert("idempotency-key", key.parse().unwrap());
    }
    let response = temper_server::odata::handle_odata_post(
        State(state.clone()),
        Some(Extension(AuthenticatedRequestContext::new(
            TenantId::new("answers"),
            identity,
        ))),
        headers,
        Path(path.into()),
        Query(Default::default()),
        Bytes::from(serde_json::to_vec(&params).unwrap()),
    )
    .await
    .into_response();
    let status = response.status();
    // Read all of it, so an answer over the budget is measured rather than cut off.
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, body)
}

#[tokio::test]
async fn an_answer_stays_in_budget_however_large_its_moves_params_were() {
    let state = fixture();
    // 40 moves of 256 KiB each: 10 MiB of params in the record's recent events.
    let text = "x".repeat(256 * 1024);
    for n in 0..40 {
        let (status, body) = post(
            &state,
            "Drafts('d1')/Probe.Write",
            json!({ "text": text }),
            None,
        )
        .await;
        assert!(
            status.is_success(),
            "write {n}: {status}: {}",
            String::from_utf8_lossy(&body[..body.len().min(500)])
        );
    }

    let mut first_answer: Option<Value> = None;
    for attempt in 0..2 {
        let (status, body) = post(
            &state,
            "Drafts('d1')/Probe.Say",
            json!({ "said": "done" }),
            Some("say-once"),
        )
        .await;
        assert!(status.is_success(), "attempt {attempt}: {status}");
        assert!(
            body.len() < ADMISSION_ANSWER_BUDGET,
            "attempt {attempt}: the answer is {} bytes, over an admission's {ADMISSION_ANSWER_BUDGET}",
            body.len()
        );
        let answer: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            answer["events"],
            json!([]),
            "attempt {attempt}: an answer carries no events, as a read carries none"
        );
        assert_eq!(
            answer["fields"]["said"],
            json!("done"),
            "the answer is still the record after the move"
        );
        if let Some(first) = &first_answer {
            for field in ["fields", "status", "sequence_nr", "total_event_count"] {
                assert_eq!(
                    answer[field], first[field],
                    "a retry must not apply the move twice"
                );
            }
        } else {
            first_answer = Some(answer);
        }
    }
}
