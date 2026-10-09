//! Explicit identity creation and interrupted client setup over the real HTTP router.

use super::*;
use temper_platform::{
    PlatformState, bootstrap_operator_credential, bootstrap_operator_credential_specs,
};
use temper_runtime::persistence::EventStore;
use temper_server::StorageStack;
use temper_store_turso::TursoEventStore;

struct Fixture {
    admin: SetupAdmin,
    store: TursoEventStore,
    server: tokio::task::JoinHandle<()>,
    directory: PathBuf,
    state: temper_server::ServerState,
}

impl Fixture {
    async fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("setup-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let store = TursoEventStore::new(
            &format!("file:{}", directory.join("events.db").display()),
            None,
        )
        .await
        .unwrap();
        let mut platform = PlatformState::new(None);
        platform
            .server
            .set_storage_stack(StorageStack::from_turso(store.clone()));
        bootstrap_operator_credential_specs(&platform, "default")
            .await
            .unwrap();
        bootstrap_operator_credential(&platform, "fixture-operator", "default")
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = platform.server.clone();
        let router = temper_platform::router::build_platform_router(platform);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let admin = SetupAdmin::new(&base, "default", "fixture-operator".into()).unwrap();
        admin
            .request(
                Method::POST,
                "/api/tenants/default/policies/rules",
                &admin.key,
                Some(json!({"rule": OPERATOR_IDENTITY_POLICY})),
            )
            .await
            .unwrap();
        Self {
            admin,
            store,
            server,
            directory,
            state,
        }
    }

    async fn create(&self, set: &str, id: &str, mut fields: Value) -> Value {
        fields["Id"] = json!(id);
        self.admin
            .request(
                Method::POST,
                &format!("/tdata/{set}"),
                &self.admin.key,
                Some(fields),
            )
            .await
            .unwrap()
    }

    async fn entity(&self, set: &str, id: &str) -> Value {
        self.admin
            .entity(&format!("/tdata/{set}('{id}')"))
            .await
            .unwrap()
            .unwrap()
    }

    async fn events(&self, entity_type: &str, id: &str) -> Value {
        serde_json::to_value(
            self.store
                .read_events(&format!("default:{entity_type}:{id}"), 0)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    fn new_admin(&self) -> SetupAdmin {
        SetupAdmin::new(&self.admin.base, "default", self.admin.key.clone()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn type_fields() -> Value {
    json!({"name":REQUESTER_TYPE,"system_prompt":"External MCP requester",
        "tool_set":"none","model":"none","max_turns":"0",
        "adapter_config":"{}","default_budget_cents":"0"})
}

fn identity(fixture: &Fixture) -> RequesterIdentity {
    RequesterIdentity {
        server: fixture.admin.base.clone(),
        tenant: "default".into(),
        principal: "mcp-recovery-probe".into(),
        token: "tmpr_recovery_probe".into(),
    }
}

fn credential_fields(identity: &RequesterIdentity) -> Value {
    json!({"agent_type_id":REQUESTER_TYPE,"agent_instance_id":identity.principal,
        "key_hash":format!("{:x}", Sha256::digest(identity.token.as_bytes())),
        "key_prefix":identity.token.chars().take(8).collect::<String>(),
        "description":"Human-authorized MCP requester","created_by":"operator","expires_at":""})
}

fn assert_fields(entity: &Value, expected: &Value) {
    for (field, value) in expected.as_object().unwrap() {
        assert_eq!(
            &entity["fields"][field], value,
            "field {field} did not survive creation/setup"
        );
    }
}

#[tokio::test]
async fn interrupted_type_creation_resumes_define_once() {
    let fixture = Fixture::new().await;
    let expected = type_fields();
    let created = fixture
        .create("AgentTypes", REQUESTER_TYPE, expected.clone())
        .await;
    assert_eq!(created["status"], "Draft");
    assert_fields(&created, &expected);
    assert_eq!(
        fixture
            .events("AgentType", REQUESTER_TYPE)
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // The first client stopped after POST-create, before Define. A fresh client resumes.
    let restarted = fixture.new_admin();
    restarted.ensure_type().await.unwrap();
    let entity = fixture.entity("AgentTypes", REQUESTER_TYPE).await;
    assert_eq!(entity["status"], "Active");
    assert_fields(&entity, &expected);
    let events = fixture.events("AgentType", REQUESTER_TYPE).await;
    assert_eq!(events[0]["event_type"], "Created");
    assert_eq!(events[1]["event_type"], "Define");
    assert_eq!(
        events.as_array().unwrap().len(),
        2,
        "one create and one Define"
    );
    restarted.ensure_type().await.unwrap();
    assert_eq!(fixture.events("AgentType", REQUESTER_TYPE).await, events);
}

#[tokio::test]
async fn absent_credential_is_created_before_issue_and_repeated_setup_is_read_only() {
    let fixture = Fixture::new().await;
    // Isolate Issue's missing-target regression from Define's.
    fixture
        .create("AgentTypes", REQUESTER_TYPE, type_fields())
        .await;
    fixture
        .admin
        .request(
            Method::POST,
            &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
            &fixture.admin.key,
            Some(type_fields()),
        )
        .await
        .unwrap();
    let identity = identity(&fixture);
    let expected = credential_fields(&identity);
    let hash = expected["key_hash"].as_str().unwrap();
    fixture.admin.ensure_credential(&identity).await.unwrap();
    let entity = fixture.entity("AgentCredentials", hash).await;
    assert_eq!(entity["status"], "Active");
    assert_fields(&entity, &expected);
    let events = fixture.events("AgentCredential", hash).await;
    assert_eq!(events[0]["event_type"], "Created");
    assert_eq!(events[1]["event_type"], "Issue");
    assert_eq!(
        events.as_array().unwrap().len(),
        2,
        "one create and one Issue"
    );
    fixture
        .new_admin()
        .ensure_credential(&identity)
        .await
        .unwrap();
    assert_eq!(fixture.events("AgentCredential", hash).await, events);
}

#[tokio::test]
async fn interrupted_credential_creation_reuses_binding_without_issue() {
    let fixture = Fixture::new().await;
    fixture
        .create("AgentTypes", REQUESTER_TYPE, type_fields())
        .await;
    fixture
        .admin
        .request(
            Method::POST,
            &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
            &fixture.admin.key,
            Some(type_fields()),
        )
        .await
        .unwrap();
    let identity = identity(&fixture);
    let expected = credential_fields(&identity);
    let hash = expected["key_hash"].as_str().unwrap();
    let created = fixture
        .create("AgentCredentials", hash, expected.clone())
        .await;
    assert_eq!(created["status"], "Active");
    assert_fields(&created, &expected);
    let events = fixture.events("AgentCredential", hash).await;
    assert_eq!(events.as_array().unwrap().len(), 1);
    // Credentials start Active: the complete create already persisted the binding.
    let restarted = fixture.new_admin();
    restarted.ensure_credential(&identity).await.unwrap();
    restarted.ensure_credential(&identity).await.unwrap();
    assert_eq!(fixture.events("AgentCredential", hash).await, events);
    assert_fields(&fixture.entity("AgentCredentials", hash).await, &expected);
    let resolved = restarted.resolve(&identity.token).await.unwrap();
    assert_eq!(resolved["verified"], true);
    assert_eq!(resolved["agent_type_name"], REQUESTER_TYPE);
    assert_eq!(resolved["agent_instance_id"], identity.principal);
}

#[tokio::test]
async fn incompatible_type_is_never_overwritten() {
    for (status, field) in [
        ("Draft", "name"),
        ("Draft", "system_prompt"),
        ("Active", "name"),
        ("Deprecated", "name"),
    ] {
        let fixture = Fixture::new().await;
        let mut fields = type_fields();
        fields[field] = json!("foreign");
        fixture
            .create("AgentTypes", REQUESTER_TYPE, fields.clone())
            .await;
        if status != "Draft" {
            fixture
                .admin
                .request(
                    Method::POST,
                    &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
                    &fixture.admin.key,
                    Some(fields),
                )
                .await
                .unwrap();
        }
        if status == "Deprecated" {
            assert!(
                fixture
                    .state
                    .dispatch_tenant_action(
                        &Default::default(),
                        "AgentType",
                        REQUESTER_TYPE,
                        "Deprecate",
                        json!({}),
                        &Default::default()
                    )
                    .await
                    .unwrap()
                    .success
            );
        }
        let before = fixture.events("AgentType", REQUESTER_TYPE).await;
        assert!(
            fixture
                .admin
                .ensure_type()
                .await
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );
        assert_eq!(fixture.events("AgentType", REQUESTER_TYPE).await, before);
    }
}

#[tokio::test]
async fn foreign_or_inactive_credential_is_never_overwritten() {
    for (status, field) in [
        ("Active", "agent_instance_id"),
        ("Active", "key_prefix"),
        ("Revoked", "agent_instance_id"),
    ] {
        let fixture = Fixture::new().await;
        fixture
            .create("AgentTypes", REQUESTER_TYPE, type_fields())
            .await;
        fixture
            .admin
            .request(
                Method::POST,
                &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
                &fixture.admin.key,
                Some(type_fields()),
            )
            .await
            .unwrap();
        let identity = identity(&fixture);
        let mut fields = credential_fields(&identity);
        let hash = fields["key_hash"].as_str().unwrap().to_owned();
        fields[field] = json!("foreign");
        fixture.create("AgentCredentials", &hash, fields).await;
        if status == "Revoked" {
            assert!(
                fixture
                    .state
                    .dispatch_tenant_action(
                        &Default::default(),
                        "AgentCredential",
                        &hash,
                        "Revoke",
                        json!({}),
                        &Default::default()
                    )
                    .await
                    .unwrap()
                    .success
            );
        }
        let before = fixture.events("AgentCredential", &hash).await;
        assert!(fixture.admin.ensure_credential(&identity).await.is_err());
        assert_eq!(fixture.events("AgentCredential", &hash).await, before);
    }
}

#[tokio::test]
async fn active_matching_type_keeps_its_existing_configuration() {
    let fixture = Fixture::new().await;
    let mut fields = type_fields();
    fields["system_prompt"] = json!("previously customized");
    fixture
        .create("AgentTypes", REQUESTER_TYPE, fields.clone())
        .await;
    fixture
        .admin
        .request(
            Method::POST,
            &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
            &fixture.admin.key,
            Some(fields.clone()),
        )
        .await
        .unwrap();
    let before = fixture.events("AgentType", REQUESTER_TYPE).await;
    fixture.admin.ensure_type().await.unwrap();
    assert_eq!(fixture.events("AgentType", REQUESTER_TYPE).await, before);
    assert_fields(&fixture.entity("AgentTypes", REQUESTER_TYPE).await, &fields);
}

#[path = "setup_race_test.rs"]
mod races;
