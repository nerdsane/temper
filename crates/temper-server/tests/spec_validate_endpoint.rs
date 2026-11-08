#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::{ServerState, SpecRegistry, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn test_state_with_registry() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("spec-validate-endpoint-test"), registry)
}

fn authenticated_post(uri: &str, body: &str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    request
}

#[tokio::test]
async fn validate_ioa_runs_server_cascade_without_loading_spec() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({
                "ioa_source": ORDER_IOA,
                "sim_seeds": 1,
                "prop_test_cases": 10
            })
            .to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], true);
}

#[tokio::test]
async fn validate_ioa_accepts_valid_source_over_default_body_limit() {
    // Grow only the text, keeping the model and its verification cost unchanged.
    let ioa_source = format!("{ORDER_IOA}\n# {}\n", "x".repeat(3 * 1024 * 1024));
    assert!(ioa_source.len() > 2 * 1024 * 1024);
    let request_body = serde_json::json!({
        "ioa_source": ioa_source,
        "sim_seeds": 1,
        "prop_test_cases": 10
    })
    .to_string();
    let response = build_router(test_state_with_registry())
        .oneshot(authenticated_post("/api/specs/validate-ioa", &request_body))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], true, "{json}");
}

#[tokio::test]
async fn validate_ioa_rejects_empty_source() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": "" }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// A spec whose model check runs for minutes with no budget.
const HEAVY_FEED: &str = include_str!("../../../test-fixtures/budget/heavy_feed.toml");

/// The endpoint passes `budget_ms` to the cascade and answers with what
/// stopped it. Unbudgeted, this spec takes minutes, so it always stops; which
/// level it stops in depends on how fast the machine is, and is checked with
/// a clock that moves only when read in temper-verify's budget tests.
#[tokio::test]
async fn validate_ioa_answers_a_spec_too_large_to_check_within_its_budget() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": HEAVY_FEED, "budget_ms": 1000 }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], false);
    let stopped = json["stopped"]
        .as_str()
        .expect("stopped names the type and why");
    assert!(
        stopped.starts_with("HeavyFeed: ") && stopped.contains(" STOPPED: "),
        "{stopped}"
    );
}

#[tokio::test]
async fn validate_ioa_rejects_a_budget_out_of_range() {
    for budget_ms in [0, 120_001] {
        let app = build_router(test_state_with_registry());
        let response = app
            .oneshot(authenticated_post(
                "/api/specs/validate-ioa",
                &serde_json::json!({ "ioa_source": ORDER_IOA, "budget_ms": budget_ms }).to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{budget_ms}");
    }
}

#[tokio::test]
async fn validate_ioa_answers_a_move_with_too_many_parameter_choices() {
    let flags: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
    let ioa = format!(
        "[automaton]\nname = \"Flags\"\nstates = [\"On\"]\ninitial = \"On\"\n\n{}\n[[action]]\nname = \"Set\"\nkind = \"input\"\nfrom = [\"On\"]\nparams = [{}]\neffect = [{}]\n\n[[invariant]]\nname = \"Held\"\nassert = \"{}\"\n",
        flags
            .iter()
            .map(|f| format!("[[state]]\nname = \"{f}\"\ntype = \"bool\"\ninitial = false\n"))
            .collect::<Vec<_>>()
            .join("\n"),
        flags
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", "),
        flags
            .iter()
            .map(|f| format!("\"{f} = params.{f}\""))
            .collect::<Vec<_>>()
            .join(", "),
        // The model holds only the fields some rule reads.
        flags
            .iter()
            .map(|f| format!("({f} || !{f})"))
            .collect::<Vec<_>>()
            .join(" && "),
    );
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": ioa }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an answer about the spec, not a worker panic"
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let stopped = json["stopped"]
        .as_str()
        .expect("stopped names the type and why");
    assert!(
        stopped.starts_with("Flags: L1 Model Check FAILED: Set takes 8192"),
        "{stopped}"
    );
}

/// A note that says why it closed, in words no rule reads.
const NOTE: &str = include_str!("../../../test-fixtures/kept/note.ioa.toml");

/// The answer `state`'s validate-ioa gives `ioa`.
async fn validate(state: &ServerState, ioa: &str) -> serde_json::Value {
    let response = build_router(state.clone())
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": ioa }).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn validate_ioa_keeps_a_passing_verdict_for_a_change_no_level_reads() {
    let state = test_state_with_registry();

    let checked = validate(&state, NOTE).await;
    assert_eq!(checked["all_passed"], true, "{checked}");
    assert!(
        checked.get("kept").is_none(),
        "the first check runs: {checked}"
    );

    let reworded = NOTE.replace("Closed by its owner", "Closed by whoever owns it");
    let kept = validate(&state, &reworded).await;
    assert_eq!(kept["all_passed"], true);
    let digest = kept["kept"].as_str().expect("the answer says it was kept");
    assert_eq!(digest.len(), 64, "{digest}");
    assert_eq!(kept["levels"], checked["levels"]);

    let guarded = NOTE.replace("guard = \"count < 2\"", "guard = \"count < 1\"");
    let rechecked = validate(&state, &guarded).await;
    assert_eq!(rechecked["all_passed"], true, "{rechecked}");
    assert!(
        rechecked.get("kept").is_none(),
        "a guard is read, so its change is checked: {rechecked}"
    );
}

/// A move that stays in place, has no guard and writes a field no rule
/// reads keeps the note's verdict; one from a place the note never reaches
/// keeps its digest, but its own check refuses it, so it is checked.
#[tokio::test]
async fn validate_ioa_keeps_a_verdict_for_a_silent_move_only_where_it_stands() {
    let state = test_state_with_registry();
    let note = NOTE.replace(
        "states = [\"Open\", \"Closed\"]",
        "states = [\"Open\", \"Closed\", \"Gone\"]",
    );
    assert_ne!(note, NOTE);
    let remark = |from: &str| {
        format!(
            "{note}\n[[state]]\nname = \"memo\"\ntype = \"string\"\ninitial = \"\"\n\n[[action]]\nname = \"Remark\"\nkind = \"input\"\nfrom = [\"{from}\"]\nparams = [{{ name = \"memo\", type = \"string\" }}]\n"
        )
    };
    let checked = validate(&state, &note).await;
    assert_eq!(checked["all_passed"], true, "{checked}");

    let kept = validate(&state, &remark("Open")).await;
    assert_eq!(kept["all_passed"], true, "{kept}");
    assert!(kept["kept"].is_string(), "a silent move keeps it: {kept}");

    let dead = validate(&state, &remark("Gone")).await;
    assert_eq!(dead["all_passed"], false, "{dead}");
    assert!(dead.get("kept").is_none(), "{dead}");
}

#[tokio::test]
async fn validate_ioa_checks_a_failing_spec_again() {
    let state = test_state_with_registry();
    let broken = NOTE.replace("assert = \"count <= 2\"", "assert = \"count <= 1\"");
    assert_ne!(broken, NOTE);
    for attempt in 0..2 {
        let answer = validate(&state, &broken).await;
        assert_eq!(answer["all_passed"], false, "attempt {attempt}: {answer}");
        assert!(answer.get("kept").is_none(), "attempt {attempt}: {answer}");
    }
}

#[path = "spec_validate_endpoint/queue.rs"]
mod queue;
