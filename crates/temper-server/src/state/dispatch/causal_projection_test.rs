//! Causal reads under projection lag, injected failures and idempotent recovery.
use super::*;
use crate::state::DispatchExtOptions;
use crate::storage::QueryPlaneStore;
use serde_json::json;
use std::sync::atomic::Ordering;
use temper_runtime::scheduler::install_deterministic_context;

#[path = "causal_projection_test/fixture_test.rs"]
mod fixture;
use fixture::fixture;

#[tokio::test]
async fn causal_submit_reaction_wasm_reads_committed_source_under_projection_lag() {
    for seed in 0..8 {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, query, queue, _temp) = fixture().await;
        let params = if seed % 2 == 0 {
            json!({"prompt_template":"new"})
        } else {
            let edit = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Source",
                    "source",
                    "Edit",
                    json!({"prompt_template":"new"}),
                    &AgentContext::system(),
                )
                .await
                .unwrap();
            assert!(edit.success);
            // A retry-style transition changes no source fields, but its
            // declared reader still requires the previously committed edit.
            json!({})
        };
        let mut events = state.event_tx.subscribe();
        let response = state
            .dispatch_tenant_action_ext(
                &TenantId::default(),
                "Source",
                "source",
                "Submit",
                params,
                DispatchExtOptions {
                    agent_ctx: &AgentContext::system(),
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap();
        assert!(response.success, "{response:?}");
        let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.entity_type == "Verifier"
                    && matches!(event.status.as_str(), "SawNew" | "SawOld")
                {
                    break event.status;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            observed, "SawNew",
            "seed {seed}: verifier observed stale source"
        );
        // A delayed older queued write cannot replace the visible commit.
        queue.enqueue_upsert(
            "default".into(),
            "Source".into(),
            "source".into(),
            "Draft".into(),
            json!({"prompt_template":"old"}),
            json!({}),
            0,
            "test_stale",
        );
        queue.drain_once_for_test().await;
        let rows = query
            .load_entity_catalog_rows("default", "Source", &["source".into()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rows[0].fields["prompt_template"], "new");
    }
}

#[tokio::test]
async fn causal_projection_failure_preserves_commit_without_starting_dependents() {
    let (state, query, _queue, _temp) = fixture().await;
    query.fail_source_write.store(true, Ordering::SeqCst);
    let mut agent = AgentContext::system();
    agent.idempotency_key = Some("same-committed-submit".into());
    let response = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Source",
            "source",
            "Submit",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &agent,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(!response.success);
    assert!(response.error.as_deref().unwrap().contains("committed"));
    let source = state
        .get_tenant_entity_state(&TenantId::default(), "Source", "source")
        .await
        .unwrap();
    assert_eq!(source.state.fields["prompt_template"], "new");
    let child = state
        .get_tenant_entity_state(&TenantId::default(), "Verifier", "verifier")
        .await
        .unwrap();
    assert_eq!(child.state.status, "Idle");
    let initial_child_sequence = child.state.sequence_nr;
    let mut events = state.event_tx.subscribe();
    query.fail_source_write.store(false, Ordering::SeqCst);
    let recovered = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Source",
            "source",
            "Submit",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &agent,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(recovered.success, "{recovered:?}");
    assert_eq!(
        recovered.state.sequence_nr, source.state.sequence_nr,
        "retry must not append another source transition"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.entity_type == "Verifier" && event.status == "SawNew" {
                break;
            }
        }
    })
    .await
    .unwrap();
    let replayed = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Source",
            "source",
            "Submit",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &agent,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(replayed.success);
    assert_eq!(replayed.state.sequence_nr, source.state.sequence_nr);
    let child = state
        .get_tenant_entity_state(&TenantId::default(), "Verifier", "verifier")
        .await
        .unwrap();
    assert_eq!(child.state.status, "SawNew");
    assert_eq!(
        child.state.sequence_nr,
        initial_child_sequence + 2,
        "one Verify and one WASM callback only"
    );
}

#[tokio::test]
async fn causal_barrier_does_not_make_ordinary_edits_synchronous() {
    let (state, query, queue, _temp) = fixture().await;
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Source",
            "source",
            "Edit",
            json!({"prompt_template":"edited"}),
            &AgentContext::system(),
        )
        .await
        .unwrap();
    assert!(response.success);
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows[0].fields["prompt_template"], "old",
        "ordinary projection stays queued"
    );
    queue.drain_once_for_test().await;
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows[0].fields["prompt_template"], "edited");
}

#[tokio::test]
async fn causal_delete_keeps_older_queued_writes_before_removal() {
    let (state, query, queue, _temp) = fixture().await;
    let mut events = state.event_tx.subscribe();
    let edited = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Source",
            "source",
            "Edit",
            json!({"prompt_template":"older queued"}),
            &AgentContext::system(),
        )
        .await
        .unwrap();
    assert!(edited.success);
    let deleted = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Source",
            "source",
            "Delete",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &AgentContext::system(),
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(deleted.success, "{deleted:?}");
    let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.entity_type == "Verifier"
                && matches!(event.status.as_str(), "SawNew" | "SawOld")
            {
                break event.status;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(observed, "SawNew");
    queue.drain_once_for_test().await;
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert!(
        rows.is_empty(),
        "older queued write must not resurrect deleted projection"
    );
}

#[tokio::test]
async fn causal_projection_timeout_keeps_commit_and_stops_dependents() {
    let (state, query, _queue, _temp) = fixture().await;
    query.hang_source_write.store(true, Ordering::SeqCst);
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let submit = state.dispatch_tenant_action_ext(
        &tenant,
        "Source",
        "source",
        "Submit",
        json!({"prompt_template":"new"}),
        DispatchExtOptions {
            agent_ctx: &agent,
            await_integration: true,
            await_reactions: true,
        },
    );
    let expire = async {
        query.projection_started.notified().await;
        // Pause only after real journal I/O and actor processing have finished.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let (response, ()) = tokio::join!(submit, expire);
        response
    })
    .await;
    tokio::time::resume();
    let response = result.expect("projection barrier must be bounded").unwrap();
    assert!(!response.success);
    let error = response.error.as_deref().unwrap();
    assert!(
        error.contains("committed") && error.contains("timed out"),
        "{error}"
    );
    let source = state
        .get_tenant_entity_state(&tenant, "Source", "source")
        .await
        .unwrap();
    assert_eq!(source.state.fields["prompt_template"], "new");
    let child = state
        .get_tenant_entity_state(&tenant, "Verifier", "verifier")
        .await
        .unwrap();
    assert_eq!(child.state.status, "Idle");
}
