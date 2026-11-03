//! The serve-first tests' world: a store holding many tenants, a simulation
//! clock that outlives each server process, and a server booted the way
//! `temper serve` boots.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime,
//! and `sim_now()` reads that paused clock ([`ProcessClock`]), so the timer
//! deadlines production code computes and the sleeps it waits on share one
//! virtual time. Dropping a runtime stops its tasks as a process exit would.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext, platform_keys};
use temper_platform::state::PlatformState;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, SimClock, SimContextGuard, install_sim_context,
};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::{PolicyStore, StorageStack};
use temper_store_turso::TursoEventStore;

use super::super::boot;
use super::gate::{Gate, GatedPolicies};

/// Time for work already under way to end: a request reaching the point
/// where it waits, or a timer that fell due firing.
pub(super) const SETTLE: Duration = Duration::from_millis(100);

/// `Fire` counts its runs and stays in `Armed`, so the timer fires once.
const PROBE_IOA: &str = r#"
[automaton]
name = "Probe"
states = ["Idle", "Armed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Armed"]
strict_action_params = true

[[state]]
name = "fires"
type = "counter"
initial = 0

[[action]]
name = "Arm"
from = ["Idle"]
to = "Armed"
params = []

[[action]]
name = "Note"
from = ["Armed"]
params = []

[[action]]
name = "Fire"
from = ["Armed"]
params = []
effect = ["fires += 1"]

[[state_timeout]]
state = "Armed"
after_seconds = 4
on_timeout = "Fire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Probe" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Probe"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Arm" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<Action Name="Note" IsBound="true"><Parameter Name="bindingParameter" Type="Probe.Probe"/></Action>
<EntityContainer Name="Container">
<EntitySet Name="Probes" EntityType="Probe.Probe"/>
</EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Each tenant's only policy: these tests check restore order, not Cedar.
/// A tenant served before it is loaded answers with a default-deny 403.
const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// Simulation time: the paused clock of the running process, carried
/// across processes.
struct ProcessClock {
    epoch: DateTime<Utc>,
    base: Mutex<(Option<tokio::time::Instant>, Duration)>,
}

impl ProcessClock {
    fn begin(&self) {
        self.base.lock().expect("clock lock").0 = Some(tokio::time::Instant::now());
    }

    fn end(&self) {
        let mut base = self.base.lock().expect("clock lock");
        let ran = base
            .0
            .take()
            .map(|origin| origin.elapsed())
            .unwrap_or_default();
        base.1 += ran;
    }
}

impl SimClock for ProcessClock {
    fn now(&self) -> DateTime<Utc> {
        let base = self.base.lock().expect("clock lock");
        let ran = base.0.map(|origin| origin.elapsed()).unwrap_or_default();
        self.epoch
            + chrono::Duration::from_std(base.1 + ran).expect("virtual time fits a chrono duration")
    }

    fn tick(&self) -> u64 {
        0
    }
}

/// What outlives a server process: the store and the simulation clock.
/// Installs the seed's simulation context on this thread until dropped.
pub(super) struct World {
    dir: tempfile::TempDir,
    clock: Arc<ProcessClock>,
    _sim: SimContextGuard,
}

impl World {
    /// An empty store, with ids drawn from `seed`.
    pub(super) fn new(seed: u64) -> Self {
        let clock = Arc::new(ProcessClock {
            epoch: Utc
                .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .expect("a valid date"),
            base: Mutex::new((None, Duration::ZERO)),
        });
        let sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        Self {
            dir: tempfile::tempdir().expect("temporary directory"),
            clock,
            _sim: sim,
        }
    }

    fn url(&self) -> String {
        format!("file:{}", self.dir.path().join("temper.db").display())
    }

    /// A process's runtime, its clock started.
    pub(super) fn process(&self) -> tokio::runtime::Runtime {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("test runtime");
        runtime.block_on(async { self.clock.begin() });
        runtime
    }

    /// End a process: its clock stops and every task it ran stops.
    pub(super) fn end(&self, runtime: tokio::runtime::Runtime) {
        runtime.block_on(async { self.clock.end() });
        drop(runtime);
    }

    /// Store `tenants`, each with the probe type and a permit-all policy,
    /// and in each of `armed` a record `p1` whose timer is armed, as an
    /// earlier process left them.
    pub(super) fn store_tenants(&self, tenants: &[String], armed: &[String]) {
        let runtime = self.process();
        runtime.block_on(async {
            let store = TursoEventStore::new(&self.url(), None)
                .await
                .expect("open the test store");
            let hash = temper_store_turso::spec_content_hash(PROBE_IOA);
            for tenant in tenants {
                store
                    .upsert_specs_and_commit(
                        tenant,
                        &[("Probe", PROBE_IOA, CSDL, &hash)],
                        None,
                        "probe",
                    )
                    .await
                    .expect("store the tenant specs");
                PolicyStore::save_policy(&store, tenant, "probe:permit", PERMIT_ALL, "fixture")
                    .await
                    .expect("store the tenant policy");
            }
            if !armed.is_empty() {
                arm_probes(store, armed).await;
            }
        });
        self.end(runtime);
    }

    /// Boot a server on this world's store the way `temper serve` does,
    /// its policy reads going through `gate`. Returns once it would answer.
    pub(super) async fn boot(&self, gate: Arc<Gate>) -> PlatformState {
        let mut stack = StorageStack::from_turso(
            TursoEventStore::new(&self.url(), None)
                .await
                .expect("open the test store"),
        );
        let inner = stack
            .policies
            .take()
            .expect("the Turso stack keeps policies");
        stack.policies = Some(Arc::new(GatedPolicies {
            inner,
            gate: gate.clone(),
        }));
        let boot::Stored {
            registry,
            policy_seed,
            mut later,
        } = boot::read_stored(None, Some(&stack), &[])
            .await
            .expect("read the stored tenants");
        let mut state = PlatformState::with_registry(registry, None);
        state.server.data_dir = self.dir.path().to_path_buf();
        state.server.set_storage_stack(stack);
        boot::restore_own(&state, policy_seed, &[], &[], &mut later)
            .await
            .expect("restore the own tenants");
        boot::start_serving(&state, later).await;
        gate.serving();
        state
    }
}

/// Create `p1` in each of `tenants` and arm its timer, on a server that
/// knows only those tenants.
async fn arm_probes(store: TursoEventStore, tenants: &[String]) {
    let mut registry = SpecRegistry::new();
    for tenant in tenants {
        registry.register_tenant(
            tenant.as_str(),
            temper_spec::csdl::parse_csdl(CSDL).expect("probe CSDL parses"),
            CSDL.to_string(),
            &[("Probe", PROBE_IOA)],
        );
    }
    let mut state = ServerState::from_registry_shared(
        ActorSystem::new("serve-first-fixture"),
        Arc::new(RwLock::new(registry)),
    );
    state.set_storage_stack(StorageStack::from_turso(store));
    for tenant in tenants {
        state
            .authz
            .reload_tenant_policies(tenant, PERMIT_ALL)
            .expect("probe policy parses");
    }
    state.rebuild_reaction_dispatcher();
    let _boot = state.start_runtime().await;
    let who = AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "fixture-agent",
            PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    };
    for tenant in tenants {
        let tenant = TenantId::new(tenant);
        state
            .get_or_create_tenant_entity(&tenant, "Probe", "p1", json!({}))
            .await
            .expect("create p1");
        let armed = state
            .dispatch_tenant_action(&tenant, "Probe", "p1", "Arm", json!({}), &who)
            .await
            .expect("arm p1");
        assert!(armed.success, "arm p1 in {tenant}: {:?}", armed.error);
    }
}

/// The actions `p1`'s journal in `tenant` holds, oldest first, read from
/// the store, not through the server.
pub(super) async fn journal(state: &PlatformState, tenant: &str) -> Vec<String> {
    let stack = state
        .server
        .storage_stack
        .as_ref()
        .expect("the server has a store");
    stack
        .events
        .read_events(&format!("{tenant}:Probe:p1"), 0)
        .await
        .expect("read the journal")
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

/// A request to `tenant` at `uri`, carrying a one-time credential for an
/// operator of that tenant.
pub(super) fn request(
    state: &PlatformState,
    tenant: &str,
    method: Method,
    uri: &str,
) -> Request<Body> {
    let operator = SecurityContext::from_resolved_identity("serve-first-test", "operator", None);
    with_credential(state, tenant, operator, method, uri)
}

/// The call a module makes back into the server from a run, timer or
/// reaction: a one-time credential for the module, bound to the tenant it
/// enters, as the module host issues it.
pub(super) fn loopback_request(
    state: &PlatformState,
    tenant: &str,
    method: Method,
    uri: &str,
) -> Request<Body> {
    let mut module = SecurityContext::from_resolved_identity("probe_module", "wasm_module", None);
    module
        .context_attrs
        .insert(platform_keys::MODULE.to_string(), json!("probe_module"));
    with_credential(state, tenant, module, method, uri)
}

fn with_credential(
    state: &PlatformState,
    tenant: &str,
    who: SecurityContext,
    method: Method,
    uri: &str,
) -> Request<Body> {
    let credential = state
        .server
        .internal_invocation_credentials
        .issue_for_url(
            AuthenticatedRequestContext::new(TenantId::new(tenant), who),
            method.as_str(),
            &format!("http://127.0.0.1:3000{uri}"),
        )
        .expect("issue a call credential");
    let body = if method == Method::POST {
        Body::from("{}")
    } else {
        Body::empty()
    };
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {credential}"))
        .header("x-tenant-id", tenant)
        .header("content-type", "application/json")
        .body(body)
        .expect("build the request")
}

/// A request to `tenant` at `uri` with no credential.
pub(super) fn public_request(tenant: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("x-tenant-id", tenant)
        .body(Body::empty())
        .expect("build the request")
}

/// What a client sees of a response.
#[derive(Debug, PartialEq)]
pub(super) struct Answer {
    pub(super) status: StatusCode,
    pub(super) body: String,
    pub(super) retry_after: Option<String>,
}

/// Send `request` through `router` and read the whole answer.
pub(super) async fn answer(router: axum::Router, request: Request<Body>) -> Answer {
    use tower::ServiceExt;
    let response = router.oneshot(request).await.expect("the router answers");
    let status = response.status();
    let retry_after = header(response.headers(), "retry-after");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read the answer body");
    Answer {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
        retry_after,
    }
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The body of an answer as JSON.
pub(super) fn json_body(answer: &Answer) -> Value {
    serde_json::from_str(&answer.body)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {} {}", answer.status, answer.body))
}
