use super::*;

#[tokio::test(flavor = "current_thread")]
async fn removing_guards_during_inference_rejects_the_old_attempt() {
    for seed in 0..32 {
        let _context = install_deterministic_context(1_200 + seed);
        let sim = SimEventStore::no_faults(1_200 + seed);
        let provider = TestProvider::new(seed, Answer::Positive);
        let state = server(sim.clone(), provider.clone());
        let tenant = TenantId::new(TENANT);
        let actor = state
            .get_or_spawn_tenant_actor(&tenant, "Case", ENTITY)
            .unwrap();
        let _: EntityResponse = actor
            .ask(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .unwrap();
        let table = state
            .registry
            .read()
            .unwrap()
            .get_table_live(&tenant, "Case")
            .unwrap();
        *provider.mutation.lock().unwrap() = Some(Mutation::RemoveGuards(table));

        let result = dispatch(&state, TENANT, "Escalate", json!({}), "old-attempt").await;
        assert!(
            rejected(&result),
            "seed={seed}: removing guards accepted evidence from the old spec"
        );
        assert_eq!(read(&state, TENANT).await.state.status, "Open");
        assert_eq!(provider.calls(), 1);
        assert!(
            sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}"))
                .iter()
                .all(|event| event.event_type != "Escalate"),
            "seed={seed}: stale attempt entered the domain journal"
        );
        assert!(rejected(
            &dispatch(&state, TENANT, "Escalate", json!({}), "old-attempt").await
        ));
        assert_eq!(provider.calls(), 1, "seed={seed}: retry resampled");

        let fresh = dispatch(&state, TENANT, "Escalate", json!({}), "fresh-attempt")
            .await
            .unwrap();
        assert!(fresh.success, "seed={seed}: fresh unguarded action refused");
        assert_eq!(fresh.state.status, "Escalated");
        assert_eq!(provider.calls(), 1);
    }
}
