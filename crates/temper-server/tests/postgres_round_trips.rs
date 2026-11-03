//! What a move and a filtered list read cost in Postgres round trips.
//!
//! The server talks to Postgres through a wire proxy that counts the client
//! messages that wait for an answer: a simple `Query` ('Q') and a `Sync` ('S')
//! that ends an extended-protocol batch. sqlx's pool pings, when it hands a
//! connection out and when it takes it back, are bare Syncs and are counted
//! apart from statements (`Cost`). A move writes its event and every row it
//! changes in one statement, and a list read reads one page, so neither cost
//! grows with the number of records of the type.
//!
//! Gated on DATABASE_URL (skips without it, runs in CI); the store works in a
//! schema of its own.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::{PostgresEventStore, PostgresSchema};
use tower::ServiceExt;

#[path = "postgres_round_trips/wire.rs"]
mod wire;
use wire::{RoundTrips, proxy};

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Panels" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Surface"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="Status" Type="Edm.String"/>
<Property Name="app" Type="Edm.String"/>
<Property Name="kind" Type="Edm.String"/>
<Property Name="body" Type="Edm.String"/>
</EntityType>
<Action Name="Keep" IsBound="true"><Parameter Name="bindingParameter" Type="Panels.Surface"/>
<Parameter Name="app" Type="Edm.String"/><Parameter Name="kind" Type="Edm.String"/></Action>
<Action Name="Edit" IsBound="true"><Parameter Name="bindingParameter" Type="Panels.Surface"/>
<Parameter Name="body" Type="Edm.String"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Surfaces" EntityType="Panels.Surface"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

const SURFACE: &str = r#"
[automaton]
name = "Surface"
states = ["Draft", "Kept"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Kept"]

[[action]]
name = "Keep"
kind = "input"
from = ["Draft"]
to = "Kept"
params = ["app", "kind"]

[[action]]
name = "Edit"
kind = "input"
from = ["Kept"]
params = ["body"]
"#;

const POLICY: &str = r#"permit(principal, action, resource is Surface);"#;

/// The filtered first page an app's view asks for: one app's kept surfaces of
/// one kind.
const LIST: &str = "/tdata/Surfaces?$filter=app%20eq%20'app-3'%20and%20status%20eq%20'Kept'%20and%20kind%20eq%20'react'&$top=1";

async fn server(url: &str, schema: &PostgresSchema) -> (ServerState, RoundTrips) {
    let options: PgConnectOptions = url.parse().unwrap();
    let upstream = (options.get_host().to_string(), options.get_port());
    let (port, trips) = proxy(upstream).await;
    // One connection: every statement goes over it in turn, so the count is
    // the requests' own and nothing opens a connection mid-measurement.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(
            options
                .host("127.0.0.1")
                .port(port)
                .ssl_mode(PgSslMode::Disable),
        )
        .await
        .unwrap();
    temper_store_postgres::migration::run_migrations_in_schema(&pool, schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool, schema.clone());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Surface", SURFACE)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("pg-round-trips"), registry);
    // Writes through the router wait for the spec to verify; this one is
    // marked verified, as the other router tests do.
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Surface",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-11-02T00:00:00Z".to_string(),
        }),
    );
    let mut stack = StorageStack::from_postgres(store);
    // No background writer shares the connection inside a counted window.
    // Trajectory entries are written in the background, off the request path,
    // so the sink is off. The snapshot queue writes a record only at its
    // 100-event boundary (`TEMPER_SNAPSHOT_INTERVAL`), which no record here
    // nears. The OTS outbox reads its queue once when the stack is attached;
    // `quiet` waits that out before the first record is made.
    stack.trajectory = None;
    state.set_storage_stack(stack);
    state
        .authz
        .reload_tenant_policies(TenantId::default().as_str(), POLICY)
        .unwrap();
    (state, trips)
}

fn authenticated(mut request: Request<Body>) -> Request<Body> {
    let security_context = temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "round-trips".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "postgres-round-trips".to_string(),
        cause: None,
        credential: None,
    };
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

/// Wait until the connection has been idle for half a second, so no earlier
/// statement is still in flight when a window opens.
async fn quiet(trips: &RoundTrips) {
    let mut last = trips.now();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = trips.now();
        if now == last {
            return;
        }
        last = now;
    }
    panic!("the connection never went idle");
}

/// What one request cost, from its start until the connection is idle again:
/// the statements it ran, and the pool's pings around them. sqlx 0.8 pings a
/// connection when the pool hands it out (`test_before_acquire`, on by
/// default) and again when the pool takes it back (`return_to_pool` in
/// sqlx-core 0.8.6 `pool/connection.rs`, always), so each use of the pool
/// costs its statements and two pings. The release ping runs in a task sqlx
/// spawns when the connection is dropped, so the request does not wait for
/// it, but it can land before or after the answer: counting until the
/// connection is idle counts it every time. The pings stay as they are; the
/// design leaves them to a change of their own.
#[derive(Debug)]
struct Cost {
    statements: usize,
    pings: usize,
    messages: Vec<String>,
}

impl Cost {
    fn of(messages: Vec<String>) -> Self {
        let pings = messages
            .iter()
            .filter(|message| message.starts_with("S (bare Sync"))
            .count();
        Self {
            statements: messages.len() - pings,
            pings,
            messages,
        }
    }

    fn total(&self) -> usize {
        self.statements + self.pings
    }
}

/// Send `request` and return its status and JSON body, and what it cost.
async fn send(
    state: &ServerState,
    trips: &RoundTrips,
    request: Request<Body>,
) -> (StatusCode, serde_json::Value, Cost) {
    quiet(trips).await;
    let before = trips.now();
    let response = build_router(state.clone())
        .oneshot(authenticated(request))
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    quiet(trips).await;
    let cost = Cost::of(trips.since(before));
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body, cost)
}

async fn edit(state: &ServerState, trips: &RoundTrips, id: &str, body: &str) -> Cost {
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("/tdata/Surfaces('{id}')/Panels.Edit"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "body": body }).to_string()))
        .unwrap();
    let (status, response, cost) = send(state, trips, request).await;
    assert_eq!(status, StatusCode::OK, "Edit {id}: {response}");
    cost
}

async fn list(state: &ServerState, trips: &RoundTrips) -> Cost {
    let request = Request::builder().uri(LIST).body(Body::empty()).unwrap();
    let (status, body, cost) = send(state, trips, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let page = body["value"].as_array().expect("a page of surfaces");
    assert_eq!(page.len(), 1, "{body}");
    assert_eq!(page[0]["fields"]["app"], "app-3", "{body}");
    assert_eq!(page[0]["fields"]["kind"], "react", "{body}");
    cost
}

/// Make surfaces `from..to`, kept, spread over ten apps and two kinds: the
/// tens alternate between the kinds, so every app holds both.
async fn keep_surfaces(state: &ServerState, from: usize, to: usize) {
    let tenant = TenantId::default();
    let agent = temper_server::request_context::AgentContext::for_service("round-trips");
    for i in from..to {
        let id = format!("s-{i:03}");
        state
            .get_or_create_tenant_entity(&tenant, "Surface", &id, json!({}))
            .await
            .unwrap();
        let kind = if (i / 10) % 2 == 0 { "react" } else { "html" };
        let kept = state
            .dispatch_tenant_action(
                &tenant,
                "Surface",
                &id,
                "Keep",
                json!({ "app": format!("app-{}", i % 10), "kind": kind }),
                &agent,
            )
            .await
            .unwrap();
        assert!(kept.success, "Keep {id}: {:?}", kept.error);
    }
}

/// On main a Postgres move wrote its event, then its catalog row, field-index
/// rows and delta mark statement by statement after the commit, and every list
/// read asked Postgres which records lacked a catalog row, 32 ids a query, so
/// both costs grew with the record and the type.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_and_a_filtered_page_cost_a_fixed_number_of_round_trips() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let schema = PostgresSchema::new(format!("round_trips_{}", sim_uuid().simple())).unwrap();
    let (state, trips) = server(&url, &schema).await;
    tokio::time::timeout(Duration::from_secs(600), async {
        quiet(&trips).await;
        keep_surfaces(&state, 0, 40).await;
        // Warm up: the first use of each statement prepares it.
        edit(&state, &trips, "s-003", "first").await;
        edit(&state, &trips, "s-003", "second").await;
        let moved = edit(&state, &trips, "s-003", "third").await;
        eprintln!("warm move:\n  {}", moved.messages.join("\n  "));
        // The move's one statement writes its event and every row with it;
        // its two pings are the pool's, for the one connection it took.
        assert_eq!(
            (moved.statements, moved.pings),
            (1, 2),
            "a warm move runs one statement on one pooled connection: {moved:#?}"
        );

        // The page reads skip + top + 1 = 2 rows (`pagination.rs`, one past
        // the page to know whether another follows). The equality read first
        // probes the field index for the matching ids, in one statement whose
        // LIMIT is the candidate budget, not the record count. When the probe
        // finds at most those 2 rows, it hydrates them itself: 2 statements.
        // When it finds more, it hands the page to the native pager, whose
        // uniform page reads only those 2 rows in one statement before they
        // are hydrated: 3 statements, with no loop, whatever the count. Of
        // the records made here, app-3 and react hold 2 at 40, 15 at 300 and
        // 150 at 3000, so 40 takes the first path and 300 and 3000 the second.
        list(&state, &trips).await;
        let at_40 = list(&state, &trips).await;
        keep_surfaces(&state, 40, 300).await;
        list(&state, &trips).await;
        let at_300 = list(&state, &trips).await;
        keep_surfaces(&state, 300, 3000).await;
        list(&state, &trips).await;
        let at_3000 = list(&state, &trips).await;
        for (records, cost) in [(40, &at_40), (300, &at_300), (3000, &at_3000)] {
            eprintln!(
                "list at {records}: {} round trips\n  {}",
                cost.total(),
                cost.messages.join("\n  ")
            );
        }
        assert_eq!(
            (at_40.statements, at_40.pings),
            (2, 4),
            "with no more matches than the page reads, the probe's ids are hydrated: {at_40:#?}"
        );
        assert_eq!(
            (at_300.statements, at_300.pings),
            (3, 6),
            "past them, the probe hands the page to the native pager: {at_300:#?}"
        );
        // The proof that the second path does not grow: ten times the records
        // and ten times the matches cost the same statements and pings.
        assert_eq!(
            (at_3000.statements, at_3000.pings),
            (at_300.statements, at_300.pings),
            "a filtered first page costs the same at 300 and 3000 records: {at_300:#?} {at_3000:#?}"
        );
    })
    .await
    .expect("the workload finishes");
    drop(state);
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
}
