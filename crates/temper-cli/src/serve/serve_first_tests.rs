//! A server with many tenants serves before it restores them all.
//!
//! The boot restores only the server's own tenants before it answers, and
//! restores every other tenant after, one at a time. A request for a tenant
//! still starting moves it to the front and waits for it, then gets the
//! answer the restored tenant gives: never a 404, an empty list, a 401 or a
//! default-deny 403. That holds for a call a module makes back into the
//! server from another tenant's run, timer or reaction as much as for an
//! outside request, and however long the wait outlasts the call's one-time
//! credential. A tenant whose restore cannot read its store answers 503
//! starting at once. A tenant's timers wait for its restore.
//!
//! Each policy read goes through a [`gate::Gate`]: the test holds a tenant's
//! restore at the read of its Cedar policies, the last thing a request needs
//! to be answered right, and lets it through when it chooses.

mod gate;
mod world;

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use serde_json::json;
use temper_runtime::scheduler::DeterministicRng;
use temper_runtime::tenant::TenantId;
use temper_server::internal_invocation::INTERNAL_INVOCATION_CREDENTIAL_TTL;

use gate::Gate;
use world::{
    Answer, SETTLE, World, answer, journal, json_body, loopback_request, public_request, request,
};

/// Seeds for the request scenario. A seed picks which starting tenants the
/// requests ask for, and the ids.
const SEEDS: u64 = 8;

/// Tenants the request scenario holds at their policy read.
const HELD: usize = 4;

/// Tenants other than the server's own in a store, named apart from them.
fn tenants(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("t-{i:03}")).collect()
}

/// Boot on a store with `others` tenants besides the server's own. Returns
/// the tenants whose policies the boot read before it served, and how many
/// of the others had specs registered by then.
fn boot_with(others: usize) -> (BTreeSet<String>, usize) {
    let world = World::new(1);
    let others = tenants(others);
    world.store_tenants(&others, &[]);
    let runtime = world.process();
    let read = runtime.block_on(async {
        let (gate, _waits) = Gate::new(&[], &[]);
        let state = world.boot(gate.clone()).await;
        let registry = state.registry.read().unwrap();
        let registered = others
            .iter()
            .filter(|t| {
                registry
                    .get_spec(&TenantId::new(t.as_str()), "Probe")
                    .is_some()
            })
            .count();
        (gate.read_before_serving(), registered)
    });
    world.end(runtime);
    read
}

#[test]
fn serve_first_a_boot_restores_only_its_own_tenants_before_it_serves() {
    let (many, registered) = boot_with(150);
    let others: Vec<&String> = many.iter().filter(|t| t.starts_with("t-")).collect();
    assert!(
        others.is_empty() && registered == 0,
        "{} tenants other than the server's own had their policies read, and {registered} \
         their specs registered, before it served: {others:?}",
        others.len(),
    );
    let (one, _) = boot_with(1);
    assert_eq!(
        one, many,
        "what a boot reads before it serves must not grow with the tenants it holds"
    );
}

#[test]
fn serve_first_a_request_for_a_starting_tenant_waits_for_it_and_gets_the_right_answer() {
    for seed in 0..SEEDS {
        eprintln!("seed {seed}");
        requests_for_starting_tenants(seed);
    }
}

fn requests_for_starting_tenants(seed: u64) {
    let world = World::new(seed);
    let held = tenants(HELD);
    let broken = "t-unreadable".to_string();
    let mut stored = held.clone();
    stored.push(broken.clone());
    world.store_tenants(&stored, &held);

    let runtime = world.process();
    runtime.block_on(async {
        let (gate, mut waits) = Gate::new(&held, std::slice::from_ref(&broken));
        let state = world.boot(gate.clone()).await;
        let router = temper_platform::router::build_platform_routes(state.clone());

        let health = answer(router.clone(), public_request("default", "/healthz")).await;
        assert_eq!(health.status, StatusCode::OK, "{health:?}");
        let health = json_body(&health);
        assert_eq!(
            health["tenants"]["starting"],
            json!(stored.len()),
            "the server serves while its other tenants are still starting: {health}"
        );

        // A timer that falls due while its tenant is starting waits for it.
        tokio::time::sleep(Duration::from_secs(30)).await;
        for tenant in &held {
            assert!(
                !journal(&state, tenant).await.contains(&"Fire".to_string()),
                "{tenant}'s timer fired before its tenant was restored"
            );
        }

        let mut rng = DeterministicRng::new(seed ^ 0x5e7e_f125);
        let mut starting = held.clone();
        while starting.len() > 1 {
            let current = waits.recv().await.expect("the restore reads a held tenant");
            starting.retain(|t| *t != current);
            let target = starting.remove(rng.next_bound(starting.len()));
            let asked = ask(&state, &router, &target);

            tokio::time::sleep(SETTLE).await;
            assert!(
                asked.iter().all(|(_, task)| !task.is_finished()),
                "a request for {target} was answered while it was starting"
            );
            gate.release(&current);
            assert_eq!(
                waits.recv().await.as_ref(),
                Some(&target),
                "a request for {target} did not move it to the front"
            );
            // Longer than a call's one-time credential lives: a waiting call
            // has no deadline, so its credential must not run out under it.
            tokio::time::sleep(INTERNAL_INVOCATION_CREDENTIAL_TTL + SETTLE).await;
            assert!(
                asked.iter().all(|(_, task)| !task.is_finished()),
                "a request for {target} was answered before its policies were read"
            );
            gate.release(&target);

            for (what, task) in asked {
                check_answer(&state, &router, &target, what, task.await.unwrap()).await;
            }
            tokio::time::sleep(SETTLE).await;
            let fires = journal(&state, &target)
                .await
                .into_iter()
                .filter(|action| action == "Fire")
                .count();
            assert_eq!(fires, 1, "{target}'s timer fires once after its restore");
        }
        for tenant in &held {
            gate.release(tenant);
        }

        for sent in [
            request(&state, &broken, Method::GET, "/tdata/Probes"),
            loopback_request(&state, &broken, Method::GET, "/tdata/Probes"),
        ] {
            let refused =
                tokio::time::timeout(Duration::from_secs(25), answer(router.clone(), sent))
                    .await
                    .expect(
                        "a request for a tenant whose restore cannot read its store does not hang",
                    );
            assert_eq!(
                refused.status,
                StatusCode::SERVICE_UNAVAILABLE,
                "{refused:?}"
            );
            assert!(
                refused.body.contains("TenantStarting") && refused.body.contains(&broken),
                "the refusal names the starting tenant: {refused:?}"
            );
            assert!(
                refused.retry_after.is_some(),
                "the refusal says when to retry: {refused:?}"
            );
        }
    });
    world.end(runtime);
}

/// What the requests for one tenant ask.
#[derive(Clone, Copy, Debug)]
enum Asked {
    List,
    Record,
    Metadata,
    Action,
    /// A module's call back into the server, from a run, timer or reaction
    /// of a tenant already restored.
    Loopback,
}

/// Send each kind of request for `tenant`, each in its own task.
fn ask(
    state: &temper_platform::state::PlatformState,
    router: &axum::Router,
    tenant: &str,
) -> Vec<(Asked, tokio::task::JoinHandle<Answer>)> {
    [
        Asked::List,
        Asked::Record,
        Asked::Metadata,
        Asked::Action,
        Asked::Loopback,
    ]
    .into_iter()
    .map(|what| {
        let sent = sent(state, tenant, what);
        (what, tokio::spawn(answer(router.clone(), sent)))
    })
    .collect()
}

fn sent(
    state: &temper_platform::state::PlatformState,
    tenant: &str,
    what: Asked,
) -> axum::http::Request<axum::body::Body> {
    match what {
        Asked::List => request(state, tenant, Method::GET, "/tdata/Probes"),
        Asked::Record => request(state, tenant, Method::GET, "/tdata/Probes('p1')"),
        Asked::Metadata => public_request(tenant, "/tdata/$metadata"),
        Asked::Action => request(
            state,
            tenant,
            Method::POST,
            "/tdata/Probes('p1')/Probe.Note",
        ),
        Asked::Loopback => loopback_request(state, tenant, Method::GET, "/tdata/Probes"),
    }
}

/// The answer a request got while its tenant was starting is the one a
/// restored tenant gives.
async fn check_answer(
    state: &temper_platform::state::PlatformState,
    router: &axum::Router,
    tenant: &str,
    what: Asked,
    got: Answer,
) {
    assert_eq!(got.status, StatusCode::OK, "{what:?} for {tenant}: {got:?}");
    match what {
        Asked::List | Asked::Loopback => {
            let items = json_body(&got)["value"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            assert_eq!(items.len(), 1, "{tenant} lists its one record: {got:?}");
            assert!(got.body.contains("\"p1\""), "{got:?}");
        }
        Asked::Record => assert!(got.body.contains("\"p1\""), "{got:?}"),
        Asked::Metadata => {
            let again = answer(router.clone(), sent(state, tenant, what)).await;
            assert_eq!(
                got, again,
                "{tenant}'s $metadata while starting is its $metadata"
            );
            assert!(got.body.contains("EntityType Name=\"Probe\""), "{got:?}");
        }
        Asked::Action => {}
    }
}
