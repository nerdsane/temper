//! WASM dispatch → callback end-to-end integration test.
//!
//! Exercises the full ServerState chain:
//! action → custom_effects → dispatch_wasm_integrations → WasmEngine.invoke()
//! → callback dispatched → entity state transitions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use temper_runtime::ActorSystem;
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ClaimSchemaVerification, ClaimSchemaVerificationOutcome,
    SchemaBundleRecord, SchemaDeploymentStore, SchemaExecutionPin, SchemaOperationIdentity,
    SchemaScope, SchemaScopeKind, SchemaVerificationReceipt, SubmitSchemaBundle,
};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::{DispatchExtOptions, PendingDecision};
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

/// Pre-built echo integration WASM binary.
const ECHO_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/echo_integration.wasm");
const LOCAL_TDATA_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/local_tdata_integration.wasm");

/// IOA spec with a `trigger echo_call` effect and WASM integration.
const ECHO_IOA: &str = r#"
[automaton]
name = "EchoTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "TriggerEcho"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = "trigger echo_call"
hint = "Kicks off the echo integration."

[[action]]
name = "EchoSucceeded"
kind = "input"
from = ["Pending"]
to = "Done"
hint = "Callback from successful echo WASM module."

[[action]]
name = "EchoFailed"
kind = "input"
from = ["Pending"]
to = "Failed"
hint = "Callback from failed echo WASM module."

[[integration]]
name = "echo_integration"
trigger = "echo_call"
type = "wasm"
module = "echo_integration"
on_success = "EchoSucceeded"
on_failure = "EchoFailed"
"#;

/// Minimal CSDL with EchoTest entity type.
const ECHO_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.EchoTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EchoTest">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="EchoTests" EntityType="Temper.EchoTest.EchoTest"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn build_echo_test_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(ECHO_CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        ECHO_CSDL_XML.to_string(),
        &[("EchoTest", ECHO_IOA)],
    );

    let system = ActorSystem::new("wasm-dispatch-test");
    ServerState::from_registry(system, registry)
}

async fn persist_active_scoped_echo_bundle(
    store: &impl SchemaDeploymentStore,
    tenant: &TenantId,
    scope: &SchemaScope,
    digest: &str,
) {
    store
        .submit_schema_bundle(SubmitSchemaBundle {
            bundle: SchemaBundleRecord {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                digest: digest.to_string(),
                predecessor_digest: None,
                canonical_csdl: ECHO_CSDL_XML.to_string(),
                canonical_ioa: std::collections::BTreeMap::from([(
                    "EchoTest".to_string(),
                    ECHO_IOA.to_string(),
                )]),
                cedar_policies: std::collections::BTreeMap::new(),
                wasm_module_digests: std::collections::BTreeMap::new(),
                migration_module_name: None,
                migration_module_digest: None,
                migration_abi_version: None,
                canonical_budgets: "{}".to_string(),
            },
            idempotency_key: "scoped-echo-submit".to_string(),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            request_id: "scoped-echo-submit".to_string(),
        })
        .await
        .expect("submit scoped echo bundle");
    let claim = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            bundle_digest: digest.to_string(),
            logical_now: 1,
            lease_expires_at: 2,
            operation: SchemaOperationIdentity {
                idempotency_key: "scoped-echo-verify".to_string(),
                request_digest: format!("sha256:{}", "2".repeat(64)),
                request_id: "scoped-echo-verify".to_string(),
            },
        })
        .await
        .expect("claim scoped echo verification");
    let fence = match claim {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record.fence,
    };
    let verified = store
        .finish_schema_verification(
            tenant.as_str(),
            scope,
            digest,
            fence,
            SchemaVerificationReceipt {
                id: "scoped-echo-verification".to_string(),
                verifier_version: "test/v1".to_string(),
                input_digest: format!("sha256:{}", "3".repeat(64)),
                passed: true,
            },
        )
        .await
        .expect("finish scoped echo verification");
    store
        .activate_schema_bundle(ActivateSchemaBundle {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            bundle_digest: digest.to_string(),
            expected_predecessor: None,
            expected_fence: verified.fence,
            verification_receipt_id: "scoped-echo-verification".to_string(),
            operation: SchemaOperationIdentity {
                idempotency_key: "scoped-echo-activate".to_string(),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: "scoped-echo-activate".to_string(),
            },
        })
        .await
        .expect("activate scoped echo bundle in store");
}

/// Build a test state with a local Turso (SQLite) backend so that
/// persisted artifacts (decisions, trajectories, invocations) can be
/// queried after dispatch.
async fn build_echo_test_state_with_turso() -> ServerState {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after UNIX epoch")
        .as_nanos();
    let db_url = format!(
        "file:/tmp/temper-wasm-dispatch-test-{}-{ts}.db",
        std::process::id()
    );
    // Clean up any leftover DB + WAL/SHM files from a previous run.
    let db_path = db_url.strip_prefix("file:").unwrap_or(&db_url);
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
    let data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-wasm-dispatch-test-{}-{ts}-data",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&data_dir);
    std::fs::create_dir_all(&data_dir).expect("create local blob data dir");
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_echo_test_state();
    state.set_storage_stack(StorageStack::from_turso(turso));
    state.data_dir = data_dir;
    state
}

async fn wait_for_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    terminal_statuses: &[&str],
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let entity = state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .expect("entity should exist");
        let status = entity.state.status.clone();
        if terminal_statuses.contains(&status.as_str()) || tokio::time::Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const ADMIN_ONLY_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#;

const ECHO_HTTP_POLICY: &str = r#"
permit(
  principal is Agent,
  action == Action::"http_call",
  resource is HttpEndpoint
) when {
  context.module == "echo_integration"
};
"#;

fn install_echo_http_policy(state: &ServerState) {
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), ECHO_HTTP_POLICY)
        .expect("policy should parse");
}

fn install_non_wasm_policy(state: &ServerState) {
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("policy should parse");
}

/// Verify authz denial artifacts are persisted to Turso.
///
/// Checks that the WASM authorization denial pathway creates:
/// 1. A PendingDecision for the denied http_call action
/// 2. A trajectory entry with authz_denied flag and source=Authz
/// 3. A WASM invocation entry recording the failed invocation
async fn assert_wasm_authz_denial_artifacts(state: &ServerState, entity_id: &str) {
    let turso = state
        .platform_turso_store()
        .expect("Turso backend required for authz denial artifact verification");

    // 1. Verify PendingDecision was persisted.
    let mut decision = None;
    for _ in 0..100 {
        let decisions = turso
            .query_all_decisions(None)
            .await
            .expect("query decisions from Turso");
        decision = decisions
            .iter()
            .filter_map(|data| serde_json::from_str::<PendingDecision>(data).ok())
            .find(|d| d.resource_id == "echo_integration" && d.action == "http_call");
        if decision.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let decision = decision.expect("expected wasm authz pending decision in Turso");
    assert_eq!(decision.module_name.as_deref(), Some("echo_integration"));

    // 2. Verify authz trajectory entry was persisted.
    let mut authz_traj = None;
    for _ in 0..100 {
        let trajectories = turso
            .load_recent_trajectories("default", 1000)
            .await
            .expect("query trajectories from Turso");
        authz_traj = trajectories
            .iter()
            .find(|t| {
                t.entity_id == entity_id
                    && t.authz_denied == Some(true)
                    && t.source.as_deref() == Some("Authz")
            })
            .cloned();
        if authz_traj.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let authz_traj = authz_traj.expect("expected authz trajectory in Turso");
    assert_eq!(
        authz_traj.denied_module.as_deref(),
        Some("echo_integration")
    );

    // 3. Verify denied WASM invocation was persisted.
    // The wasm_invocation_logs table does not store authz_denied directly;
    // we identify the denied invocation by entity_id + failure + error text.
    let mut denied_invocation = None;
    for _ in 0..100 {
        let invocations = turso
            .load_recent_wasm_invocations(1000)
            .await
            .expect("query wasm invocations from Turso");
        denied_invocation = invocations
            .iter()
            .find(|w| {
                w.entity_id == entity_id
                    && !w.success
                    && w.error
                        .as_deref()
                        .is_some_and(|e| e.contains("authorization denied"))
            })
            .cloned();
        if denied_invocation.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let denied_invocation = denied_invocation.expect("expected denied wasm invocation in Turso");
    assert_eq!(denied_invocation.module_name, "echo_integration");
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_integration_dispatches_callback() {
    let state = build_echo_test_state();
    let tenant = TenantId::default();
    install_echo_http_policy(&state);

    // Register the WASM module in the engine and module registry.
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    {
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, "echo_integration", &hash);
    }

    // Dispatch TriggerEcho — should succeed and emit custom effect "echo_call".
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-1",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("TriggerEcho should succeed");

    assert!(response.success, "TriggerEcho should succeed");
    assert_eq!(response.state.status, "Pending");
    assert!(
        response.custom_effects.contains(&"echo_call".to_string()),
        "should emit echo_call effect, got: {:?}",
        response.custom_effects
    );

    // Poll for the callback to be dispatched asynchronously.
    // The WASM module is invoked in a tokio::spawn task, and its callback
    // (EchoSucceeded or EchoFailed) is dispatched back to the entity actor.
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-1",
        &["Done", "Failed"],
        Duration::from_secs(45),
    )
    .await;

    // The echo module calls https://echo.example.com/ping via ProductionWasmHost.
    // ProductionWasmHost makes a real HTTP call that will fail (DNS resolution).
    // The echo module handles HTTP failure gracefully: it returns "-1\n" as the
    // response and still reports success with callback_action = "EchoSucceeded".
    // So the on_success callback fires, transitioning to "Done".
    assert_eq!(
        final_status, "Done",
        "entity should transition to Done after WASM callback (echo module returns success even on HTTP failure)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn scoped_wasm_integration_uses_the_pinned_bundle_spec() {
    let tenant = TenantId::default();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "scoped-wasm-dispatch".to_string(),
    };
    let digest = format!("sha256:{}", "a".repeat(64));
    let mut registry = SpecRegistry::new();
    registry
        .stage_scoped_bundle(
            tenant.clone(),
            scope.clone(),
            digest.clone(),
            parse_csdl(ECHO_CSDL_XML).expect("CSDL should parse"),
            ECHO_CSDL_XML.to_string(),
            &[("EchoTest", ECHO_IOA)],
        )
        .expect("scoped echo bundle should stage");
    registry
        .activate_scoped_bundle(&tenant, &scope, &digest, None)
        .expect("scoped echo bundle should activate");
    let mut state =
        ServerState::from_registry(ActorSystem::new("scoped-wasm-dispatch-test"), registry);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after UNIX epoch")
        .as_nanos();
    let db_url = format!(
        "file:/tmp/temper-scoped-wasm-dispatch-test-{}-{ts}.db",
        std::process::id()
    );
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create scoped test store");
    persist_active_scoped_echo_bundle(&store, &tenant, &scope, &digest).await;
    state.set_storage_stack(StorageStack::from_turso(store));
    install_echo_http_policy(&state);
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock")
        .register(&tenant, "echo_integration", &hash);
    let pin = SchemaExecutionPin {
        scope,
        bundle_digest: digest,
    };
    state
        .get_or_create_scoped_entity(
            &tenant,
            "EchoTest",
            "scoped-echo",
            serde_json::json!({}),
            pin.clone(),
        )
        .await
        .expect("scoped echo entity should be created");
    let successor_digest = format!("sha256:{}", "b".repeat(64));
    let successor_ioa = ECHO_IOA.replace(
        "module = \"echo_integration\"",
        "module = \"missing_scoped_integration\"",
    );
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry
            .stage_scoped_bundle(
                tenant.clone(),
                pin.scope.clone(),
                successor_digest.clone(),
                parse_csdl(ECHO_CSDL_XML).expect("CSDL should parse"),
                ECHO_CSDL_XML.to_string(),
                &[("EchoTest", successor_ioa.as_str())],
            )
            .expect("successor bundle with different integration should stage");
        registry
            .activate_scoped_bundle(
                &tenant,
                &pin.scope,
                &successor_digest,
                Some(&pin.bundle_digest),
            )
            .expect("successor bundle should become active");
    }
    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "EchoTest",
            "scoped-echo",
            "TriggerEcho",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext {
                    schema_pin: Some(pin),
                    ..AgentContext::system()
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("scoped trigger should dispatch");

    assert_eq!(response.state.status, "Done");
}

#[tokio::test(flavor = "multi_thread")]
async fn triggered_wasm_local_tdata_uses_module_not_ambient_authority() {
    let state = build_echo_test_state();
    let tenant = TenantId::default();
    let hash = state
        .wasm_engine
        .compile_and_cache(LOCAL_TDATA_WASM)
        .expect("local TData module should compile");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock")
        .register(&tenant, "echo_integration", &hash);

    let module_http_permit = r#"
        permit(principal is Agent, action == Action::"http_call", resource is HttpEndpoint) when {
            principal has role && principal.id == "echo_integration" && principal.role == "wasm_module"
        };
    "#;
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            &format!(
                r#"{module_http_permit}
                permit(principal is Agent, action == Action::"list", resource is EchoTest) when {{
                    principal.id == "ambient-operator"
                }};"#
            ),
        )
        .expect("ambient-only local TData policy should parse");
    let ambient = AgentContext {
        agent_id: Some("ambient-operator".to_string()),
        agent_type: Some("operator".to_string()),
        security_ctx: Some(temper_authz::SecurityContext::from_resolved_identity(
            "ambient-operator",
            "operator",
            None,
        )),
        ..AgentContext::default()
    };
    let denied = state
        .dispatch_tenant_action_ext(
            &tenant,
            "EchoTest",
            "triggered-local-denied",
            "TriggerEcho",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &ambient,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("triggered integration should report local denial");
    assert_eq!(denied.state.status, "Failed");

    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            &format!(
                r#"{module_http_permit}
                permit(principal is Agent, action == Action::"list", resource is EchoTest) when {{
                    principal has role && principal.id == "echo_integration" && principal.role == "wasm_module"
                }};"#
            ),
        )
        .expect("module-only local TData policy should parse");
    let allowed = state
        .dispatch_tenant_action_ext(
            &tenant,
            "EchoTest",
            "triggered-local-allowed",
            "TriggerEcho",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &ambient,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("triggered integration should use module authority");
    assert_eq!(allowed.state.status, "Done");
}

#[tokio::test(flavor = "multi_thread")]
async fn persisted_wasm_modules_are_lazy_compiled_on_first_invoke() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_echo_http_policy(&state);
    let hash = temper_wasm::WasmEngine::hash_module(ECHO_WASM);

    state
        .upsert_wasm_module("default", "echo_integration", ECHO_WASM, &hash, "bundled")
        .await
        .expect("persist echo module");
    state
        .load_wasm_modules()
        .await
        .expect("recover persisted wasm modules");

    {
        let wasm_reg = state
            .wasm_module_registry
            .read()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        assert_eq!(
            wasm_reg.get_hash(&tenant, "echo_integration"),
            Some(hash.as_str())
        );
    }
    assert!(
        !state.wasm_engine.is_cached(&hash),
        "startup recovery should register the module without eagerly compiling it"
    );

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-lazy-1",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("TriggerEcho should succeed");

    assert!(response.success, "TriggerEcho should succeed");
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-lazy-1",
        &["Done", "Failed"],
        Duration::from_secs(45),
    )
    .await;
    assert_eq!(final_status, "Done");
    assert!(
        state.wasm_engine.is_cached(&hash),
        "the first invoke should lazily compile the recovered module"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn persisted_wasm_modules_with_legacy_db_blob_fallback_execute_after_startup_restore() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_echo_http_policy(&state);
    let turso = state
        .platform_turso_store()
        .expect("turso backend required");
    let hash = temper_wasm::WasmEngine::hash_module(ECHO_WASM);

    turso
        .upsert_wasm_module("default", "echo_integration", ECHO_WASM, &hash, "bundled")
        .await
        .expect("persist metadata-only echo module");
    turso
        .put_blob(&format!("wasm-modules/{hash}"), ECHO_WASM)
        .await
        .expect("persist legacy DB-backed artifact");

    state
        .load_wasm_modules()
        .await
        .expect("recover persisted wasm modules");

    let recovered_hash = {
        let wasm_reg = state
            .wasm_module_registry
            .read()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg
            .get_hash(&tenant, "echo_integration")
            .expect("legacy DB-backed module should still be registered")
            .to_string()
    };

    assert_eq!(recovered_hash, hash);
    assert!(
        !state.wasm_engine.is_cached(&recovered_hash),
        "startup restore should still avoid eager compilation for legacy DB-backed rows"
    );

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-legacy-hash",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("TriggerEcho should succeed");

    assert!(response.success, "TriggerEcho should succeed");
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-legacy-hash",
        &["Done", "Failed"],
        Duration::from_secs(45),
    )
    .await;
    assert_eq!(final_status, "Done");
    assert!(
        state.wasm_engine.is_cached(&recovered_hash),
        "legacy DB-backed modules should still lazy-compile on first invoke"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_missing_module_dispatches_failure_callback() {
    let state = build_echo_test_state();
    let tenant = TenantId::default();

    // Do NOT register any WASM module — the module registry is empty.
    // dispatch_wasm_integrations should detect the missing module and fire on_failure.

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-missing",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("TriggerEcho should succeed (transition is valid)");

    assert!(response.success);
    assert_eq!(response.state.status, "Pending");

    // Poll for the failure callback.
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-missing",
        &["Failed", "Done"],
        Duration::from_secs(20),
    )
    .await;

    assert_eq!(
        final_status, "Failed",
        "missing module should trigger on_failure callback → Failed state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_authz_denial_records_governance_artifacts_async_mode() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_non_wasm_policy(&state);

    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    {
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, "echo_integration", &hash);
    }

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-authz-async",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("TriggerEcho should succeed");
    assert_eq!(response.state.status, "Pending");

    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-authz-async",
        &["Failed"],
        Duration::from_secs(45),
    )
    .await;
    assert_eq!(final_status, "Failed");
    assert_wasm_authz_denial_artifacts(&state, "echo-authz-async").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_authz_denial_records_governance_artifacts_blocking_mode() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_non_wasm_policy(&state);

    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    {
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, "echo_integration", &hash);
    }

    let agent_ctx = AgentContext::default();
    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "EchoTest",
            "echo-authz-blocking",
            "TriggerEcho",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("blocking TriggerEcho should return callback result");
    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
    assert_wasm_authz_denial_artifacts(&state, "echo-authz-blocking").await;
}
