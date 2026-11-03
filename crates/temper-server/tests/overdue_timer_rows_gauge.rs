//! The gauge of timer rows past their due time reads the rows in the store,
//! not the timers a process armed: a row past due that no process armed shows
//! on it. Before it, a timer that never fired was seen only when something
//! touched its record, and 20 Factory Releases sat in Queued for two days.
//!
//! Its own test binary, so the global meter provider is set before anything
//! in this process records a metric.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Gauge;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Tickets" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Ticket"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Tickets" EntityType="Tickets.Ticket"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// A `Ticket` waits a day in `Waiting`, then lapses back to `Open`.
const TICKET: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Waiting"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "Lapse"
kind = "internal"
from = ["Waiting"]
to = "Open"

[[state_timeout]]
state = "Waiting"
after_seconds = 86400
on_timeout = "Lapse"
"#;

const BUDGET: i64 = 86_400;

fn exporter() -> &'static (InMemoryMetricExporter, SdkMeterProvider) {
    static EXPORTER: OnceLock<(InMemoryMetricExporter, SdkMeterProvider)> = OnceLock::new();
    EXPORTER.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        (exporter, provider)
    })
}

async fn store(dir: &tempfile::TempDir) -> TursoEventStore {
    let url = format!("file:{}", dir.path().join("timers.db").display());
    TursoEventStore::new(&url, None).await.expect("store opens")
}

fn server(store: &TursoEventStore, tenant: &str, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL).expect("CSDL parses"),
            CSDL.to_string(),
            &[("Ticket", TICKET)],
        )
        .expect("tenant registers");
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .expect("policy parses");
    state
}

/// A `Waiting` ticket's timer row whose clock started `ago` before now.
fn row(tenant: &str, id: &str, started_at: DateTime<Utc>) -> StateTimerRecord {
    StateTimerRecord {
        tenant: tenant.to_string(),
        entity_type: "Ticket".to_string(),
        entity_id: id.to_string(),
        state: "Waiting".to_string(),
        arm_id: format!("{id}-arm"),
        sequence_nr: 2,
        started_at: started_at.to_rfc3339(),
        due_at: Some((started_at + chrono::Duration::seconds(BUDGET)).to_rfc3339()),
        params: "{}".to_string(),
        armed_by: "{}".to_string(),
    }
}

fn value_of(attributes: &[KeyValue], key: &str) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

/// The newest value of gauge `name` for `tenant`, by `kind`.
fn gauge(name: &str, tenant: &str) -> BTreeMap<String, u64> {
    let (exporter, provider) = exporter();
    provider.force_flush().expect("flush metrics");
    let mut values = BTreeMap::new();
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope.metrics.iter().filter(|m| m.name == name) {
                let gauge = metric
                    .data
                    .as_any()
                    .downcast_ref::<Gauge<u64>>()
                    .expect("a u64 gauge");
                for point in gauge
                    .data_points
                    .iter()
                    .filter(|point| value_of(&point.attributes, "tenant") == tenant)
                {
                    values.insert(value_of(&point.attributes, "kind"), point.value);
                }
            }
        }
    }
    values
}

/// Two `Waiting` tickets whose day ran out, one and two days ago, and one
/// with most of its day left. No process armed any of them: the rows are all
/// there is. The gauge counts the two past due, and the older is two days
/// late.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_row_past_due_that_no_process_armed_shows_on_the_gauge() {
    exporter();
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let tenant = "stranded";
    let now = Utc::now();
    let day = chrono::Duration::seconds(BUDGET);
    for (id, started_at) in [
        ("two-days-late", now - day * 3),
        ("a-day-late", now - day * 2),
        ("not-yet", now - chrono::Duration::hours(1)),
    ] {
        store
            .upsert_state_timer(&row(tenant, id, started_at))
            .await
            .expect("row written");
    }

    let state = server(&store, tenant, "overdue-gauge");
    state
        .record_overdue_timer_rows(Duration::from_secs(10))
        .await;

    let rows = gauge("temper_timer_rows_overdue", tenant);
    assert_eq!(
        rows.get("state_timeout"),
        Some(&2),
        "the two rows past due, not the one with time left: {rows:?}"
    );
    assert_eq!(rows.get("scheduled_action"), Some(&0), "{rows:?}");
    let oldest = gauge("temper_timer_rows_overdue_oldest_seconds", tenant);
    let late = oldest.get("state_timeout").copied().unwrap_or(0);
    let two_days = 2 * BUDGET as u64;
    assert!(
        (two_days..two_days + 60).contains(&late),
        "the oldest row is two days late, not {late} s: {oldest:?}"
    );
}

/// A row an older release wrote has no due time, so the gauge cannot see it.
/// The first boot of this release writes it, from the row's start and its
/// state's budget, and leaves the timer as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_boot_dates_a_timer_row_an_older_release_left_without_a_due_time() {
    exporter();
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir).await;
    let tenant = "upgraded";
    let id = "t1";
    {
        let first = server(&store, tenant, "older-release");
        let tenant = TenantId::new(tenant);
        first
            .get_or_create_tenant_entity(&tenant, "Ticket", id, serde_json::json!({"Id": id}))
            .await
            .expect("ticket created");
        let waited = first
            .dispatch_tenant_action(
                &tenant,
                "Ticket",
                id,
                "Wait",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("Wait dispatches");
        assert!(waited.success, "{:?}", waited.error);
    }
    let armed = store
        .state_timer_row(tenant, "Ticket", id)
        .await
        .expect("row reads")
        .expect("Wait armed a timer row");
    let due_at = armed.due_at.clone().expect("this release dates the row");
    let started_at = DateTime::parse_from_rfc3339(&armed.started_at).unwrap();
    assert_eq!(
        DateTime::parse_from_rfc3339(&due_at).unwrap(),
        started_at + chrono::Duration::seconds(BUDGET),
        "the row is due a day after Wait"
    );
    // As an older release wrote it: the same arm, no due time.
    store
        .upsert_state_timer(&StateTimerRecord {
            due_at: None,
            ..armed.clone()
        })
        .await
        .expect("row rewritten");

    let next = server(&store, tenant, "this-release");
    let walk = next.start_runtime().await.expect("boot");
    walk.await.expect("boot walk ends");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let dated = loop {
        let row = store
            .state_timer_row(tenant, "Ticket", id)
            .await
            .expect("row reads")
            .expect("the boot keeps the row");
        if row.due_at.is_some() || tokio::time::Instant::now() >= deadline {
            break row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        dated,
        StateTimerRecord {
            due_at: Some(due_at),
            ..armed
        },
        "the boot writes the row's due time and changes nothing else"
    );
}
