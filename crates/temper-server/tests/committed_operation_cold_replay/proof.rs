//! Strict positive core controls, durable corrupt/legacy proof and snapshots.
use super::fixture::*;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{persistence::EventStore, tenant::TenantId};
use temper_server::{
    EntityMsg, EntityResponse, ServerState, build_router, idempotency::ActionReplyMode,
};
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

pub(super) async fn direct(state: &ServerState, action: &str, key: &str) -> EntityResponse {
    state
        .get_or_spawn_tenant_actor(&TenantId::default(), "Work", "one")
        .unwrap()
        .ask(
            EntityMsg::Action {
                name: action.into(),
                params: json!({}),
                related: BTreeMap::new(),
                idempotency_key: Some(key.into()),
                expected_authorization_precondition: None,
                reply_mode: ActionReplyMode::DirectCore,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn proven_core_reply_survives_later_writes_and_trigger_changes() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52310);
    for (old, new) in [(false, true), (true, false)] {
        let store = SimEventStore::no_faults(52310);
        let warm = server(&store, old, "direct-core");
        assert_eq!(
            request(&warm, None, None, true).await.0,
            StatusCode::CREATED
        );
        let original = direct(&warm, "Start", "core").await;
        assert!(original.success);
        assert_eq!(original.state.status, "Running");
        assert!(direct(&warm, "Complete", "complete").await.success);
        assert!(direct(&warm, "Archive", "archive").await.success);
        assert_eq!(
            invocations(&warm),
            0,
            "direct actor call launched integrations"
        );
        let before = serde_json::to_value(store.dump_journal("default:Work:one")).unwrap();
        drop(warm);
        let cold = server(&store, new, "core-after-spec-change");
        for _ in 0..2 {
            let reply = request(&cold, Some("Start"), Some("core"), true).await;
            assert_eq!(reply.0, StatusCode::OK, "{reply:?}");
            assert_eq!(
                logical(&reply.1),
                logical(&serde_json::to_value(&original.state).unwrap())
            );
            assert_eq!(
                invocations(&cold),
                0,
                "history re-emitted current custom integration"
            );
        }
        assert_eq!(
            cold.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                .await
                .unwrap()
                .state
                .status,
            "Archived"
        );
        assert_eq!(
            serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
            before
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum Damage {
    Valid,
    Missing,
    Version,
    Shape,
    Digest,
    WrongClass,
    CoreDigest,
    Mode,
    AdditionalField,
    Null,
}
fn damage(payload: &mut Value, kind: Damage) {
    match kind {
        Damage::Valid => {}
        Damage::Missing => {
            payload.as_object_mut().unwrap().remove("idempotency_reply");
        }
        Damage::Version => payload["idempotency_reply"]["version"] = json!(99),
        Damage::Shape => payload["idempotency_reply"] = json!("not a proof"),
        Damage::Digest => payload["idempotency_reply"]["digest"] = json!("bad"),
        Damage::WrongClass => {
            payload["idempotency_reply"]["reply_class"] = json!("post_completion")
        }
        Damage::CoreDigest => payload["idempotency_result"] = json!("bad core digest"),
        Damage::Mode => payload["idempotency_reply"]["mode"]["kind"] = json!("unknown_mode"),
        Damage::AdditionalField => payload["idempotency_reply"]["unknown"] = json!(true),
        Damage::Null => payload["idempotency_reply"] = Value::Null,
    }
}

#[tokio::test]
async fn durable_proof_validation_hydrates_then_refuses_without_warming_success() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52311);
    for snapshot in [false, true] {
        for kind in [
            Damage::Valid,
            Damage::Missing,
            Damage::Version,
            Damage::Shape,
            Damage::Digest,
            Damage::WrongClass,
            Damage::CoreDigest,
            Damage::Mode,
            Damage::AdditionalField,
            Damage::Null,
        ] {
            let store = SimEventStore::no_faults(52311);
            let warm = server(&store, false, "proof-validation");
            request(&warm, None, None, true).await;
            let original = direct(&warm, "Start", "core").await;
            assert!(original.success);
            let head = direct(&warm, "Complete", "later").await;
            assert!(head.success);
            let target = SimEventStore::no_faults(52312);
            let mut events = store.dump_journal("default:Work:one");
            let start = events
                .iter_mut()
                .find(|e| e.payload["action"] == "Start")
                .unwrap();
            damage(&mut start.payload, kind);
            target.append("default:Work:one", 0, &events).await.unwrap();
            if snapshot {
                let mut state = serde_json::to_value(head.state).unwrap();
                for e in state["events"].as_array_mut().unwrap() {
                    if e["action"] == "Start" {
                        damage(e, kind);
                    }
                }
                target
                    .save_snapshot(
                        "default:Work:one",
                        events.len() as u64,
                        &serde_json::to_vec(&state).unwrap(),
                    )
                    .await
                    .unwrap();
            }
            drop(warm);
            let cold = server(&target, true, "cold-proof-validation");
            assert_eq!(
                cold.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                    .await
                    .unwrap()
                    .state
                    .status,
                "Done",
                "{snapshot}/{kind:?}: malformed proof must not prevent hydration"
            );
            let before = serde_json::to_value(target.dump_journal("default:Work:one")).unwrap();
            for _ in 0..2 {
                let reply = request(&cold, Some("Start"), Some("core"), true).await;
                if matches!(kind, Damage::Valid) {
                    assert_eq!(reply.0, StatusCode::OK, "{snapshot}/{kind:?}: {reply:?}");
                    assert_eq!(
                        logical(&reply.1),
                        logical(&serde_json::to_value(&original.state).unwrap())
                    );
                } else {
                    assert_eq!(
                        reply.0,
                        StatusCode::CONFLICT,
                        "{snapshot}/{kind:?}: {reply:?}"
                    );
                    assert_eq!(reply.1["error"]["code"], "IdempotencyKeyUnverifiable");
                    assert!(
                        reply.1["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains("reply or completion"),
                        "proven binding must not be diagnosed as unverifiable request: {reply:?}"
                    );
                }
            }
            // Even bad/legacy provenance must not replace mismatch or auth precedence.
            let mismatch = request(&cold, Some("Archive"), Some("core"), true).await;
            assert_eq!(mismatch.0, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(mismatch.1["error"]["code"], "IdempotencyKeyMismatch");
            let mut denied = Request::post("/tdata/Works('one')/Test.Start")
                .header("idempotency-key", "core")
                .body(Body::from("{}"))
                .unwrap();
            denied
                .extensions_mut()
                .insert(AuthenticatedRequestContext::new(
                    TenantId::default(),
                    SecurityContext::from_resolved_identity("intruder", "test-agent", None),
                ));
            assert_eq!(
                build_router(cold.clone())
                    .oneshot(denied)
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(invocations(&cold), 0);
            assert_eq!(
                serde_json::to_value(target.dump_journal("default:Work:one")).unwrap(),
                before
            );
            assert_eq!(
                cold.get_tenant_entity_state(&TenantId::default(), "Work", "one")
                    .await
                    .unwrap()
                    .state
                    .status,
                "Done"
            );
        }
    }
}

#[tokio::test]
async fn valid_core_marker_cannot_certify_reconstruction_under_changed_effects() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52345);
    let store = SimEventStore::no_faults(52345);
    let original_spec = spec(false)
        .replacen(
            "[[action]]",
            "[[state]]\nname=\"starts\"\ntype=\"counter\"\ninitial=0\n[[action]]",
            1,
        )
        .replacen(
            "to=\"Running\"",
            "to=\"Running\"\neffect=[\"starts += 1\"]",
            1,
        );
    let register = |state: &ServerState, source: &str| {
        let mut registry = state.registry.write().unwrap();
        registry
            .try_register_tenant_with_constraints(
                "default",
                temper_spec::csdl::parse_csdl(CSDL).unwrap(),
                CSDL.into(),
                &[("Work", source)],
                None,
                true,
            )
            .unwrap();
        registry.set_verification_status(
            &TenantId::default(),
            "Work",
            temper_server::registry::VerificationStatus::Completed(
                temper_server::registry::EntityVerificationResult {
                    all_passed: true,
                    levels: vec![],
                    verified_at: "test".into(),
                },
            ),
        );
    };
    let warm = server(&store, false, "core-original-effects");
    register(&warm, &original_spec);
    let original = direct(&warm, "Start", "core").await;
    assert!(original.success);
    assert_eq!(original.state.counters["starts"], 1);
    assert!(direct(&warm, "Complete", "later").await.success);
    drop(warm);
    let before = serde_json::to_value(store.dump_journal("default:Work:one")).unwrap();
    let cold = server(&store, false, "core-drifted-effects");
    register(&cold, &original_spec.replace("starts += 1", "starts += 2"));
    let current = cold
        .get_tenant_entity_state(&TenantId::default(), "Work", "one")
        .await
        .unwrap();
    for _ in 0..2 {
        let reply = request(&cold, Some("Start"), Some("core"), true).await;
        assert_eq!(reply.0, StatusCode::CONFLICT);
        assert_eq!(reply.1["error"]["code"], "IdempotencyKeyUnverifiable");
    }
    let after = cold
        .get_tenant_entity_state(&TenantId::default(), "Work", "one")
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(current.state).unwrap(),
        serde_json::to_value(after.state).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
        before
    );
}
