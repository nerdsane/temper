use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

fn test_state() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test");
    ServerState::new(system, csdl, csdl_xml.to_string())
}

fn test_state_with_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs)
}

fn test_state_with_order_and_payment_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa-order-payment");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    // For navigation tests we only need entity creation/read, so reuse the same minimal IOA.
    specs.insert("Payment".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs)
}

#[tokio::test]
async fn test_service_document() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["value"].is_array());
    assert_eq!(json["@odata.context"], "$metadata");
}

#[tokio::test]
async fn test_metadata_endpoint() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/$metadata")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response.headers().get("Content-Type").unwrap();
    assert_eq!(content_type, "application/xml");
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("edmx:Edmx"));
    assert!(body_str.contains("Temper.Example"));
}

#[tokio::test]
async fn test_entity_set_listing() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["@odata.context"], "$metadata#Orders");
}

#[tokio::test]
async fn test_entity_by_key_not_found() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('abc-123')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Nonexistent entity returns 404 (no transition table = no actor)
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_entity_by_key_found() {
    let app = build_router(test_state_with_ioa());

    // First create an entity via POST
    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "test-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    // Now GET the created entity
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('test-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["@odata.context"], "$metadata#Orders/$entity");
}

#[tokio::test]
async fn test_unknown_entity_set_returns_404() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/NonExistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_post_entity_creation() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"status": "Draft"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_post_bound_action() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::post("/tdata/Orders('abc-123')/Temper.Example.CancelOrder")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"Reason": "changed mind"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "Cancelled");
}

#[tokio::test]
async fn test_odata_version_header() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let odata_version = response.headers().get("OData-Version").unwrap();
    assert_eq!(odata_version, "4.0");
}

#[tokio::test]
async fn test_old_odata_path_returns_404() {
    let app = build_router(test_state());
    let response = app
        .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_post_body_used_for_entity_creation() {
    let app = build_router(test_state_with_ioa());

    // Create with specific ID and fields
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "order-42", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Verify the body fields were stored
    assert_eq!(json["fields"]["customer"], "Bob");
    assert_eq!(json["fields"]["id"], "order-42");
}

#[tokio::test]
async fn test_entity_set_returns_created_entities() {
    let app = build_router(test_state_with_ioa());

    // Create two entities
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o2", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // GET the entity set — should return both entities
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let values = json["value"].as_array().unwrap();
    assert_eq!(values.len(), 2);
}

#[tokio::test]
async fn test_patch_updates_entity() {
    let app = build_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "p1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // PATCH the entity
    let response = app
        .clone()
        .oneshot(
            Request::patch("/tdata/Orders('p1')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["fields"]["customer"], "Bob");
}

#[tokio::test]
async fn test_delete_removes_entity() {
    let app = build_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "d1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // DELETE
    let response = app
        .clone()
        .oneshot(
            Request::delete("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // GET should now return 404
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_patch_nonexistent_returns_404() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::patch("/tdata/Orders('nope')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_returns_404() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::delete("/tdata/Orders('nope')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_navigation_property_single_entity() {
    let app = build_router(test_state_with_order_and_payment_ioa());

    // Create parent order.
    let order_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(order_create.status(), StatusCode::CREATED);

    // Create related payment linked by OrderId.
    let payment_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Payments")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "pay-nav-1", "OrderId": "ord-nav-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(payment_create.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-1')/Payment")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["entity_type"], "Payment");
    assert_eq!(json["fields"]["OrderId"], "ord-nav-1");
}

#[tokio::test]
async fn test_navigation_property_not_found_returns_404() {
    let app = build_router(test_state_with_ioa());
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-missing')/DefinitelyMissingNav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_temper_client_script_served() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
    assert_eq!(
        response.headers().get("Cache-Control").unwrap(),
        "public, max-age=3600"
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("Temper"));
}

#[tokio::test]
async fn test_temper_client_script_alias_served() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/static/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
}

#[tokio::test]
async fn test_cors_header_present() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders")
                .header("Origin", "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap(),
        "*"
    );
}

const AGENT_DEFINITION_CSDL_XML: &str =
    include_str!("../../../test-fixtures/specs/agent_definition.csdl.xml");
const AGENT_DEFINITION_IOA: &str =
    include_str!("../../../test-fixtures/specs/agent_definition.ioa.toml");
const PROGRAM_DEFINITION_IOA: &str =
    include_str!("../../../test-fixtures/specs/program_definition.ioa.toml");
const PROCESS_IOA: &str = include_str!("../../../test-fixtures/specs/process.ioa.toml");

fn test_state_with_agent_definition_ioa() -> ServerState {
    let csdl = parse_csdl(AGENT_DEFINITION_CSDL_XML).unwrap();
    let system = ActorSystem::new("test-agent-platform");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert(
        "AgentDefinition".to_string(),
        AGENT_DEFINITION_IOA.to_string(),
    );
    specs.insert(
        "ProgramDefinition".to_string(),
        PROGRAM_DEFINITION_IOA.to_string(),
    );
    specs.insert("Process".to_string(), PROCESS_IOA.to_string());
    ServerState::with_specs(
        system,
        csdl,
        AGENT_DEFINITION_CSDL_XML.to_string(),
        specs,
    )
}

#[tokio::test]
async fn test_agent_definition_csdl_crud_over_tdata() {
    let app = build_router(test_state_with_agent_definition_ioa());

    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/AgentDefinitions")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "id": "ci-fixer-001",
                        "name": "ci-fixer",
                        "system_prompt": "You diagnose CI failures and propose fixes.",
                        "model_provider": "anthropic",
                        "model_name": "claude-sonnet-4-6",
                        "model_max_tokens": 8192,
                        "tools_json": "[\"datadog_logs_search\",\"bash\"]",
                        "labels_json": "{\"team\":\"ci-platform\"}",
                        "created_at": "2026-03-11T10:00:00Z",
                        "updated_at": "2026-03-11T10:00:00Z"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let get_response = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentDefinitions('ci-fixer-001')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(get_json["fields"]["name"], "ci-fixer");
    assert_eq!(
        get_json["fields"]["system_prompt"],
        "You diagnose CI failures and propose fixes."
    );

    let list_response = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentDefinitions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let values = list_json["value"].as_array().unwrap();
    assert_eq!(values.len(), 1);

    let patch_response = app
        .clone()
        .oneshot(
            Request::patch("/tdata/AgentDefinitions('ci-fixer-001')")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "system_prompt": "You diagnose CI failures, summarize root causes, and propose fixes.",
                        "updated_at": "2026-03-11T11:00:00Z"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch_response.status(), StatusCode::OK);

    let patch_body = axum::body::to_bytes(patch_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let patch_json: serde_json::Value = serde_json::from_slice(&patch_body).unwrap();
    assert_eq!(
        patch_json["fields"]["system_prompt"],
        "You diagnose CI failures, summarize root causes, and propose fixes."
    );

    let delete_response = app
        .clone()
        .oneshot(
            Request::delete("/tdata/AgentDefinitions('ci-fixer-001')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);

    let missing_response = app
        .oneshot(
            Request::get("/tdata/AgentDefinitions('ci-fixer-001')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_processes_csdl_api_only_lifecycle() {
    let app = build_router(test_state_with_agent_definition_ioa());

    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "id": "proc-001",
                        "definition_kind": "agent",
                        "definition_id": "ci-fixer-001",
                        "status": "Ready",
                        "created_at": "2026-03-11T10:00:00Z",
                        "updated_at": "2026-03-11T10:00:00Z"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let start_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.StartProcess")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "user_prompt": "Diagnose CI failures for PR #42"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(start_response.status(), StatusCode::OK);
    let start_body = axum::body::to_bytes(start_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let start_json: serde_json::Value = serde_json::from_slice(&start_body).unwrap();
    assert_eq!(start_json["status"], "Running");

    let send_input_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.SendInput")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "user_prompt": "Proceed with the fix."
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send_input_response.status(), StatusCode::OK);
    let send_input_body = axum::body::to_bytes(send_input_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let send_input_json: serde_json::Value = serde_json::from_slice(&send_input_body).unwrap();
    assert_eq!(send_input_json["status"], "Running");

    let suspend_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.SuspendProcess")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(suspend_response.status(), StatusCode::OK);
    let suspend_body = axum::body::to_bytes(suspend_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let suspend_json: serde_json::Value = serde_json::from_slice(&suspend_body).unwrap();
    assert_eq!(suspend_json["status"], "Suspended");

    let resume_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.ResumeProcess")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resume_response.status(), StatusCode::OK);
    let resume_body = axum::body::to_bytes(resume_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let resume_json: serde_json::Value = serde_json::from_slice(&resume_body).unwrap();
    assert_eq!(resume_json["status"], "Running");

    let terminate_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.TerminateProcess")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{
                        "reason": "user_requested"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(terminate_response.status(), StatusCode::OK);
    let terminate_body = axum::body::to_bytes(terminate_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let terminate_json: serde_json::Value = serde_json::from_slice(&terminate_body).unwrap();
    assert_eq!(terminate_json["status"], "Terminated");

    // Invalid transition: cannot resume after terminated.
    let invalid_resume = app
        .clone()
        .oneshot(
            Request::post("/tdata/Processes('proc-001')/Temper.AgentV1.ResumeProcess")
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_resume.status(), StatusCode::CONFLICT);

    let delete_response = app
        .clone()
        .oneshot(
            Request::delete("/tdata/Processes('proc-001')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
}
