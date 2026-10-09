//! Cold reply authority: original integration REDs plus explicit fail-closed policy.
use axum::http::StatusCode;
use temper_runtime::{scheduler::install_deterministic_context, tenant::TenantId};
use temper_store_sim::SimEventStore;
#[path = "committed_operation_cold_replay/fixture.rs"]
mod fixture;
use fixture::*;
#[path = "committed_operation_cold_replay/conflict.rs"]
mod conflict;
#[path = "committed_operation_cold_replay/proof.rs"]
mod proof;

async fn replay(
    integration: bool,
    awaited: bool,
    intervening: bool,
    remove_trigger: bool,
    seed: u64,
) {
    let (_sim, _, _) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let warm = server(&store, integration, "warm-callback");
    assert_eq!(
        request(&warm, None, None, awaited).await.0,
        StatusCode::CREATED
    );
    let first = request(&warm, Some("Start"), None, awaited).await;
    assert_eq!(first.0, StatusCode::OK, "first: {first:?}");
    let expected = if integration && awaited {
        "Done"
    } else {
        "Running"
    };
    assert_eq!(first.1["status"], expected, "first logical reply");
    if integration {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let current = warm
                    .get_tenant_entity_state(&TenantId::default(), "Work", "one")
                    .await
                    .unwrap();
                if current.state.status == "Done" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual WASM callback never completed");
        assert_eq!(invocations(&warm), 1);
    } else {
        assert_eq!(
            request(&warm, Some("Complete"), None, true).await.0,
            StatusCode::OK
        );
        assert_eq!(invocations(&warm), 0);
    }
    let journal = store.dump_journal("default:Work:one");
    let start = journal
        .iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap();
    let complete = journal
        .iter()
        .find(|e| e.payload["action"] == "Complete")
        .unwrap();
    let key = start.payload["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        start.payload["idempotency_key"], complete.payload["idempotency_key"],
        "callback must have its own actual key"
    );
    assert!(start.payload["idempotency_result"].is_string());
    if intervening {
        assert_eq!(
            request(&warm, Some("Archive"), None, true).await.0,
            StatusCode::OK
        );
    }
    let before = store.dump_journal("default:Work:one");
    let retry = request(&warm, Some("Start"), Some(&key), awaited).await;
    assert_eq!(retry.0, StatusCode::OK);
    assert_eq!(
        logical(&retry.1),
        logical(&first.1),
        "warm must keep first logical response"
    );
    assert_eq!(invocations(&warm), usize::from(integration));
    drop(warm);
    // No actor handles, response cache, receipt graph or registry are reused.
    let cold = server(&store, integration && !remove_trigger, "cold-callback");
    let retry = request(&cold, Some("Start"), Some(&key), awaited).await;
    let current = cold
        .get_tenant_entity_state(&TenantId::default(), "Work", "one")
        .await
        .unwrap();
    let after = store.dump_journal("default:Work:one");
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap(),
        "retry must append no events or effects"
    );
    assert_eq!(invocations(&cold), 0, "cold history must not reinvoke WASM");
    assert_eq!(
        current.state.status,
        if intervening { "Archived" } else { "Done" },
        "historical reply must not regress current state"
    );
    eprintln!(
        "cold-replay seed={seed} integration={integration} awaited={awaited} intervening={intervening} trigger_removed={remove_trigger} first={} warm={} cold_http={} cold_status={} current={} events={} parent_key={} callback_key={}",
        first.1["status"],
        expected,
        retry.0,
        retry.1["status"],
        current.state.status,
        after.len(),
        key,
        complete.payload["idempotency_key"]
    );
    // Approved compatibility change: dispatch outcomes require post-completion
    // proof even when the first integration mode was background or no trigger
    // ran. Strict positive direct-core controls are in proof.rs.
    assert_eq!(
        retry.0,
        StatusCode::CONFLICT,
        "dispatcher cold reply must refuse absent final proof: {retry:?}"
    );
    assert_eq!(retry.1["error"]["code"], "IdempotencyKeyUnverifiable");
    let repeated = request(&cold, Some("Start"), Some(&key), awaited).await;
    assert_eq!(
        repeated.0,
        StatusCode::CONFLICT,
        "refusal warmed an invented success"
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
}

#[tokio::test]
async fn awaited_callback_cold_reply_is_original_or_unverifiable() {
    replay(true, true, false, false, 52301).await;
}
#[tokio::test]
async fn awaited_callback_with_intervening_state_cold_reply_is_original_or_unverifiable() {
    replay(true, true, true, false, 52302).await;
}
#[tokio::test]
async fn removing_trigger_cannot_certify_an_old_callback_reply() {
    replay(true, true, true, true, 52303).await;
}
#[tokio::test]
async fn dispatcher_pure_action_cold_reply_requires_completion_proof() {
    replay(false, true, true, false, 52304).await;
}
#[tokio::test]
async fn background_first_mode_does_not_change_its_logical_reply() {
    replay(true, false, true, false, 52305).await;
}
