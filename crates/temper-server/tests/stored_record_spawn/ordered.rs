//! Ordered spawn initializers through the real actors and simulated journal.

use super::*;
use temper_runtime::scheduler::DeterministicRng;
use temper_server::storage::StorageStack;
use temper_store_sim::SimEventStore;

/// Seeded journal conflicts delay the first initializer of an existing child.
/// The second initializer must still see its committed result.
#[tokio::test(start_paused = true)]
async fn dst_repeated_spawns_check_and_store_the_same_ordered_child_updates() {
    let mut conflicts = 0;
    for seed in 0..16 {
        let (_guard, _clock, _id_gen) =
            temper_runtime::scheduler::install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let sim = SimEventStore::no_faults(seed);
        let mut state = build_state();
        state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
        for existing in [false, true] {
            for (case, action, expected) in [
                ("refused", "ArchiveThenInit", StatusCode::CONFLICT),
                ("accepted", "InitThenArchive", StatusCode::OK),
                (
                    "refused-initializer",
                    "ArchiveThenInitAllowed",
                    StatusCode::OK,
                ),
            ] {
                let id = format!("{case}-{existing}");
                if existing {
                    let (status, body) =
                        send(&state, Method::POST, "/tdata/Children", json!({"Id": id})).await;
                    assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
                }
                let (status, body) =
                    send(&state, Method::POST, "/tdata/Runs", json!({"Id": id})).await;
                assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
                let inject_conflict = existing && expected == StatusCode::OK && rng.chance(0.5);
                let persistence_id = format!("default:Child:{id}");
                if inject_conflict {
                    // GetState waits for the child's bootstrap append before injecting.
                    state
                        .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                        .await
                        .unwrap();
                    sim.inject_concurrency_violations(&persistence_id, 1);
                    conflicts += 1;
                }
                let (status, body) = send(
                    &state,
                    Method::POST,
                    &format!("/tdata/Runs('{id}')/Temper.Spawn.{action}"),
                    json!({"child_id": id}),
                )
                .await;
                assert_eq!(
                    status, expected,
                    "seed {seed}, {action}, existing={existing}: {body}"
                );
                assert_eq!(
                    stored_run(&state, &id).await.0,
                    if expected == StatusCode::OK {
                        "Running"
                    } else {
                        "Idle"
                    }
                );
                if expected == StatusCode::OK {
                    let completed =
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                if let Ok(child) = state
                                    .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                                    .await
                                    && child.state.status == "Archived"
                                {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                            }
                        })
                        .await;
                    assert!(
                        completed.is_ok(),
                        "seed {seed}, {action}: accepted spawns must store the checked child state"
                    );
                    let actions: Vec<_> = sim
                        .dump_journal(&persistence_id)
                        .into_iter()
                        .filter(|event| event.event_type != "Created")
                        .map(|event| event.event_type)
                        .collect();
                    let expected_actions = if action == "InitThenArchive" {
                        vec!["Init", "Archive"]
                    } else {
                        vec!["Archive"]
                    };
                    assert_eq!(
                        actions, expected_actions,
                        "seed {seed}: child updates commit in effect order"
                    );
                    if inject_conflict {
                        assert_eq!(
                            sim.pending_concurrency_violations(&persistence_id),
                            0,
                            "seed {seed}: the delayed initializer retried"
                        );
                    }
                } else if existing {
                    let child = state
                        .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                        .await
                        .unwrap();
                    assert_eq!(
                        child.state.status, "New",
                        "seed {seed}: refused parent leaves child unchanged"
                    );
                } else {
                    assert!(
                        !state
                            .ensure_entity_loaded(&TenantId::default(), "Child", &id)
                            .await
                    );
                }
            }
        }
    }
    assert!(
        conflicts > 0,
        "the workload must delay child initializers with journal conflicts"
    );
}
