//! Creating a record costs the same however many records are already live.
//!
//! Every spawn used to record the server's gauges by walking every live actor
//! (mailbox depth and utilization) and the whole entity index. On 27 Oct 2026
//! that walk was 87% of a create's handling at 6,000 records, and a create's
//! p50 rose from 1.58 ms to 2.44 ms between 360 and 6,060 records. The gauges
//! are now sampled by the runtime metrics loop. The shared idempotency cache
//! must not walk all actors on each insertion either: a debug assertion that
//! recounted its replies made creates grow again. Its indexes are checked in
//! deterministic cache tests instead.
//!
//! This creates records one at a time on an in-memory server and compares the
//! median CPU time of the first creates with that of the last. It counts the
//! process's CPU time, every thread's, not the time on the wall: a create's
//! work counts wherever it runs, and time this process waits while other
//! processes on a shared runner use the CPU does not (31 Oct 2026, a busy
//! runner made the wall-clock medians 0.48 ms and 1.75 ms with no walk).
//! Medians of 200 creates keep one slow create from deciding it.

use std::time::{Duration, Instant};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const SERVICE: &str = "create-time-test";

/// Records created in all.
const RECORDS: usize = 3_000;

/// Creates timed at each end.
const WINDOW: usize = 200;

/// How much more CPU the last creates may use than the first. Without the
/// walk the two medians are within noise of each other; with it, the last
/// creates walk about 3,000 actors each.
const GROWTH_ALLOWED: f64 = 3.0;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CreateTime" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Item">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Items" EntityType="Temper.CreateTime.Item"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ITEM_IOA: &str = r#"
[automaton]
name = "Item"
states = ["Open"]
initial = "Open"

[[state]]
name = "touches"
type = "counter"
initial = 0

[[action]]
name = "Touch"
kind = "input"
from = ["Open"]
effect = ["touches += 1"]
"#;

fn build_state(tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Item", ITEM_IOA)],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("create-time"), registry);
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:create-time-test", action, resource);"#,
        )
        .expect("policy should load");
    state
}

/// The CPU time every thread of this process has used so far.
fn cpu_used() -> Duration {
    let mut used = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `used` is a valid timespec for the call to write.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut used) };
    assert_eq!(status, 0, "the process CPU clock reads");
    Duration::new(used.tv_sec as u64, used.tv_nsec as u32)
}

fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

#[tokio::test(flavor = "multi_thread")]
async fn create_time_stays_flat_as_records_grow() {
    let tenant = TenantId::new("items");
    let state = build_state("items");
    let who = AgentContext::for_service(SERVICE);

    let mut times = Vec::with_capacity(RECORDS);
    for n in 0..RECORDS {
        let started = cpu_used();
        let created = state
            .dispatch_tenant_action(
                &tenant,
                "Item",
                &format!("item-{n:05}"),
                "Touch",
                serde_json::json!({}),
                &who,
            )
            .await
            .expect("Touch dispatches");
        times.push(cpu_used() - started);
        assert!(created.success, "{:?}", created.error);
    }
    assert_eq!(state.active_actor_count(), RECORDS as u64);

    let first = median(times[..WINDOW].to_vec());
    let last = median(times[RECORDS - WINDOW..].to_vec());
    eprintln!("median create: first {first:?}, last {last:?}");
    assert!(
        last.as_secs_f64() <= first.as_secs_f64() * GROWTH_ALLOWED,
        "a create used {last:?} of CPU (median) with {} records live, against {first:?} \
         with none: something each create does grows with the records",
        RECORDS - WINDOW
    );
}

/// Measure the shared reply cache directly, so the rest of dispatch cannot
/// mask work proportional to the number of actors with cached replies.
#[tokio::test]
async fn cache_insert_time_stays_flat_as_records_grow() {
    use temper_server::idempotency::{CachedReply, IdempotencyCache, RequestFingerprint};

    let state = build_state("items");
    let response = state
        .dispatch_tenant_action(
            &TenantId::new("items"),
            "Item",
            "fixture",
            "Touch",
            serde_json::json!({}),
            &AgentContext::for_service(SERVICE),
        )
        .await
        .expect("fixture dispatches");
    assert!(response.success);
    let cache = IdempotencyCache::new();
    let request = RequestFingerprint::new("Touch", &serde_json::json!({}));
    let mut times = Vec::with_capacity(RECORDS);
    for n in 0..RECORDS {
        let actor = format!("items:Item:item-{n:05}");
        let started = Instant::now(); // determinism-ok: the test measures wall time
        // A dispatch caches its actor reply, then completes its effects.
        cache.put(&actor, "create", &request, &response);
        cache.put_effects_applied(&actor, "create", &request, &response);
        times.push(started.elapsed());
    }
    assert!(matches!(
        cache.get("items:Item:item-00000", "create", &request),
        CachedReply::Replay(_)
    ));
    let first = median(times[..WINDOW].to_vec());
    let last = median(times[RECORDS - WINDOW..].to_vec());
    eprintln!("median cache insert: first {first:?}, last {last:?}");
    assert!(
        last.as_secs_f64() <= first.as_secs_f64() * GROWTH_ALLOWED,
        "cache insertion grew from {first:?} to {last:?} with {} records live",
        RECORDS - WINDOW
    );
}
