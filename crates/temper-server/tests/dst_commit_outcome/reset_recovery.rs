//! Recovery must replace a live arm superseded by a committed reset.

use super::*;

/// A reset whose commit answer and outcome read are lost must replace the
/// live pre-reset arm when the actor rebuilds from its journal.
#[test]
fn committed_reset_replaces_live_timer_after_unknown_outcome() {
    for seed in 0..16 {
        let (_guard, clock, _ids) = install_deterministic_context(seed);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(reset_recovery(seed, &clock));
    }
}

async fn reset_recovery(seed: u64, clock: &temper_runtime::scheduler::LogicalClock) {
    let store = SimEventStore::no_faults(seed);
    let tally = TALLY.replace(
        "on_timeout = \"Expire\"",
        "on_timeout = \"Expire\"\nreset_on = [\"Bump\"]\nargs = { Mark = \"notes\" }",
    );
    let tally = tally.replace("name = \"Expire\"\nkind = \"internal\"", "name = \"Expire\"\nkind = \"internal\"\nparams = [{ name = \"ExpiredBy\", type = \"string\", source = \"authenticated_subject\" }, { name = \"Mark\", type = \"counter\" }]");
    let tally = tally.replace(
        "initial = \"Open\"",
        "initial = \"Open\"\nstrict_action_params = true",
    );
    let state = server_with_tally(&store, seed, &tally);
    let tenant = TenantId::new(TENANT);
    state
        .get_or_create_tenant_entity(&tenant, "Tally", "reset", json!({"Id":"reset"}))
        .await
        .unwrap();
    let original = AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_verified_jwt(
            "original-person",
            temper_authz::PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    };
    assert!(
        state
            .dispatch_tenant_action(
                &tenant,
                "Tally",
                "reset",
                "Hold",
                json!({"Mark":"hold"}),
                &original
            )
            .await
            .unwrap()
            .success
    );
    settle().await;
    clock.advance_by(432000);
    tokio::time::advance(Duration::from_secs(43200)).await;
    settle().await;
    Fault::LandThenFailUnreadable.apply(&store, &journal_of("reset"));
    let resetter = AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_verified_jwt(
            "reset-person",
            temper_authz::PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    };
    let answer = state
        .dispatch_tenant_action(
            &tenant,
            "Tally",
            "reset",
            "Bump",
            json!({"Mark":"reset"}),
            &resetter,
        )
        .await;
    assert!(
        !answer.as_ref().is_ok_and(|response| response.success),
        "seed {seed}: {answer:?}"
    );
    Fault::clear(&store, &journal_of("reset"));
    assert_eq!(
        landed(&store, "reset", "reset").await,
        1,
        "seed {seed}: reset must have committed"
    );
    let row = store
        .list_state_timers(TENANT)
        .into_iter()
        .find(|row| row.entity_id == "reset")
        .unwrap();
    let committed_params: serde_json::Value = serde_json::from_str(&row.params).unwrap();
    // A non-resetting action forces rehydration without arming a
    // fresh timer itself, matching the next request after the error.
    assert!(
        state
            .dispatch_tenant_action(
                &tenant,
                "Tally",
                "reset",
                "Note",
                json!({}),
                &AgentContext::default()
            )
            .await
            .unwrap()
            .success
    );
    settle().await;
    clock.advance_by(432010);
    tokio::time::advance(Duration::from_secs(43201)).await;
    settle().await;
    assert_eq!(
        journal(&store, "reset").await.0.status,
        "Busy",
        "seed {seed}: old deadline fired after a committed reset"
    );
    clock.advance_by(432000);
    tokio::time::advance(Duration::from_secs(43200)).await;
    settle().await;
    assert_eq!(
        journal(&store, "reset").await.0.status,
        "Open",
        "seed {seed}: reset deadline did not fire"
    );
    let events = store.read_events(&journal_of("reset"), 0).await.unwrap();
    let expired = events
        .iter()
        .find(|event| event.event_type == "Expire")
        .unwrap();
    assert_eq!(
        expired.payload["params"]["Mark"], committed_params["Mark"],
        "seed {seed}: timer must use the committed arguments"
    );
    assert_eq!(
        expired.payload["params"]["ExpiredBy"], "reset-person",
        "seed {seed}: timer must use the committed caller"
    );
}

/// An unrelated move must retain the committed timer's deadline and payload,
/// while its replacement fences the timeout against the current journal.
#[test]
fn non_resetting_move_preserves_committed_timer() {
    for seed in 0..16 {
        let (_guard, clock, _ids) = install_deterministic_context(seed);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let store = SimEventStore::no_faults(seed);
            let tally = TALLY.replace(
                "on_timeout = \"Expire\"",
                "on_timeout = \"Expire\"\nargs = { Mark = \"notes\" }",
            );
            let tally = tally.replace("name = \"Expire\"\nkind = \"internal\"", "name = \"Expire\"\nkind = \"internal\"\nparams = [{ name = \"ExpiredBy\", type = \"string\", source = \"authenticated_subject\" }, { name = \"Mark\", type = \"counter\" }]");
            let tally = tally.replace(
                "initial = \"Open\"",
                "initial = \"Open\"\nstrict_action_params = true",
            );
            let state = server_with_tally(&store, seed, &tally);
            let tenant = TenantId::new(TENANT);
            let original = AgentContext {
                security_ctx: Some(temper_authz::SecurityContext::from_verified_jwt(
                    "original-person",
                    temper_authz::PrincipalKind::Agent,
                    Some("test"), None, None, None,
                )),
                ..Default::default()
            };
            state.get_or_create_tenant_entity(&tenant, "Tally", "keep", json!({"Id":"keep"}))
                .await.unwrap();
            assert!(state.dispatch_tenant_action(&tenant, "Tally", "keep", "Hold",
                json!({"Mark":"hold"}), &original).await.unwrap().success);
            settle().await;
            let row = store.list_state_timers(TENANT).into_iter()
                .find(|row| row.entity_id == "keep").unwrap();
            let params: serde_json::Value = serde_json::from_str(&row.params).unwrap();
            clock.advance_by(432000);
            tokio::time::advance(Duration::from_secs(43200)).await;
            settle().await;
            assert!(state.dispatch_tenant_action(&tenant, "Tally", "keep", "Bump",
                json!({"Mark":"unrelated"}), &AgentContext::default()).await.unwrap().success);
            settle().await;
            let retained = store.list_state_timers(TENANT).into_iter()
                .find(|row| row.entity_id == "keep").unwrap();
            assert_eq!(retained, row, "seed {seed}: unrelated move changed the durable arm");
            assert!(journal(&store, "keep").await.0.sequence > row.sequence_nr);
            clock.advance_by(431990);
            tokio::time::advance(Duration::from_secs(43199)).await;
            settle().await;
            assert_eq!(journal(&store, "keep").await.0.status, "Busy",
                "seed {seed}: timeout fired before the committed deadline");
            clock.advance_by(10);
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            assert_eq!(journal(&store, "keep").await.0.status, "Open",
                "seed {seed}: timer lost after a non-resetting move");
            let events = store.read_events(&journal_of("keep"), 0).await.unwrap();
            let expired: Vec<_> = events.iter().filter(|event| event.event_type == "Expire").collect();
            assert_eq!(expired.len(), 1, "seed {seed}: timeout must fire once");
            assert_eq!(expired[0].payload["params"]["Mark"], params["Mark"],
                "seed {seed}: timer must keep the committed arguments");
            assert_eq!(expired[0].payload["params"]["ExpiredBy"], "original-person",
                "seed {seed}: timer must keep the committed caller");
            assert!(store.list_state_timers(TENANT).into_iter().all(|row| row.entity_id != "keep"));
        });
    }
}
