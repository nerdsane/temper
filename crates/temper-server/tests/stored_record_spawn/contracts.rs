//! Each initializer's contract reads the child left by the preceding spawn.

use super::*;
use temper_server::storage::StorageStack;
use temper_store_sim::SimEventStore;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized", "Archived"]
initial = "New"
strict_action_params = true

[[state]]
name = "counter"
type = "counter"
initial = 0

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"
effect = ["counter += 1"]

[[action]]
name = "Archive"
kind = "input"
from = ["Initialized"]
to = "Archived"
params = [{name = "expected", type = "counter"}]
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "counter"

[[state]]
name = "touches"
type = "counter"
initial = 0

# Counts itself, so its event is written: a spawn's initializer is a machine's
# move, and one that changes nothing writes none.
[[action]]
name = "Touch"
kind = "input"
from = ["Initialized"]
effect = ["touches += 1"]
"#;

const RUN: &str = r#"
[automaton]
name = "Run"
states = ["Idle", "Running"]
initial = "Idle"

[[action]]
name = "RequireInitialized"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id", "expected"]
effect = ["spawn('Child', 'Init', ChildRef, params.child_id)", "spawn('Child', 'Archive', ChildRef, params.child_id)"]

[[action]]
name = "RequireArchived"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id", "expected"]
effect = ["spawn('Child', 'Init', ChildRef, params.child_id)", "spawn('Child', 'Archive', ChildRef, params.child_id)"]

[[action]]
name = "TryArchiveThenInit"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id", "expected"]
effect = ["spawn('Child', 'Archive', ChildRef, params.child_id)", "spawn('Child', 'Init', ChildRef, params.child_id)", "spawn('Child', 'Touch', ChildRef, params.child_id)"]
"#;

const CROSS: &str = r#"
version = 1

[[invariant]]
name = "ChildStaysInitialized"
kind = "hard"
on = "Run.RequireInitialized"
assert = 'related(Child, ChildRef).status in ["Initialized"]'

[[invariant]]
name = "ChildIsArchived"
kind = "hard"
on = "Run.RequireArchived"
assert = 'related(Child, ChildRef).status in ["Archived"]'

[[invariant]]
name = "EarlierRefusalStaysRefused"
kind = "hard"
on = "Run.TryArchiveThenInit"
assert = 'related(Child, ChildRef).status in ["Initialized"]'
"#;

#[tokio::test(start_paused = true)]
async fn dst_repeated_spawn_contracts_read_the_preceding_pending_child() {
    for seed in 0..16 {
        let (_guard, _clock, _id_gen) =
            temper_runtime::scheduler::install_deterministic_context(seed);
        let sim = SimEventStore::no_faults(seed);
        let mut state = build_state_with_specs(CHILD, RUN, CROSS);
        state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
        for existing in [false, true] {
            for (action, expected_counter, expected_status, child_status) in [
                ("RequireInitialized", 1, StatusCode::CONFLICT, "Archived"),
                ("RequireArchived", 1, StatusCode::OK, "Archived"),
                ("RequireInitialized", 0, StatusCode::OK, "Initialized"),
                ("TryArchiveThenInit", 1, StatusCode::OK, "Initialized"),
                ("RequireArchived", 0, StatusCode::CONFLICT, "Initialized"),
            ] {
                let id = format!("{action}-{expected_counter}-{existing}");
                if existing {
                    let (status, body) =
                        send(&state, Method::POST, "/tdata/Children", json!({"Id": id})).await;
                    assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
                }
                let (status, body) =
                    send(&state, Method::POST, "/tdata/Runs", json!({"Id": id})).await;
                assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
                let persistence_id = format!("default:Child:{id}");
                if expected_status == StatusCode::OK {
                    // Cover creation as well as an existing child's first
                    // initializer: later initializers must wait for recovery.
                    match seed % 3 {
                        1 => sim.fail_next_appends(&persistence_id, 1),
                        2 => {
                            sim.land_next_appends_then_fail(&persistence_id, 1);
                            if existing {
                                // The committed initializer's answer and its
                                // readback are lost: the owed retry must resume
                                // after earlier final refusals, not re-run them.
                                sim.fail_next_reads(&persistence_id, 1);
                            }
                        }
                        _ => {}
                    }
                }
                let pause_between_initializers = existing && action == "TryArchiveThenInit";
                if pause_between_initializers {
                    sim.stop_writes_after_next_append(&persistence_id);
                }
                let (status, body) = send(
                    &state,
                    Method::POST,
                    &format!("/tdata/Runs('{id}')/Temper.Spawn.{action}"),
                    json!({"child_id": id, "expected": expected_counter}),
                )
                .await;
                assert_eq!(status, expected_status, "seed {seed}, {id}: {body}");
                assert_eq!(
                    stored_run(&state, &id).await.0,
                    if expected_status == StatusCode::OK {
                        "Running"
                    } else {
                        "Idle"
                    },
                    "seed {seed}, {id}"
                );
                if pause_between_initializers {
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while sim.refused_appends(&persistence_id) == 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                    })
                    .await
                    .expect("Touch must meet the store outage after Init lands");
                    sim.take_writes_again();
                }
                if action == "TryArchiveThenInit" && expected_status == StatusCode::OK {
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while !sim
                            .dump_journal(&persistence_id)
                            .iter()
                            .any(|event| event.event_type == "Touch")
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                    })
                    .await
                    .unwrap_or_else(|_| {
                        panic!("seed {seed}, {id}: Touch must land after recovery")
                    });
                }
                if expected_status == StatusCode::OK {
                    tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        loop {
                            if let Ok(child) = state
                                .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                                .await
                                && child.state.status == child_status
                            {
                                assert_eq!(child.state.counters["counter"], 1);
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                    })
                    .await
                    .unwrap_or_else(|_| {
                        panic!("seed {seed}, {id}: child must reach {child_status}")
                    });
                    if child_status == "Initialized" {
                        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                            loop {
                                let refused = state
                                    .entity_observe_log
                                    .lock()
                                    .unwrap()
                                    .values()
                                    .flatten()
                                    .any(|event| {
                                        event.entity_id == id
                                            && event.event_name == "integration_callback_rejected"
                                            && event.data["action"] == "Archive"
                                    });
                                if refused {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                            }
                        })
                        .await
                        .unwrap_or_else(|_| panic!("seed {seed}, {id}: Archive must be refused"));
                    }
                    let child = state
                        .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                        .await
                        .unwrap();
                    assert_eq!(child.state.status, child_status, "seed {seed}, {id}");
                    let actions: Vec<_> = sim
                        .dump_journal(&persistence_id)
                        .into_iter()
                        .filter(|event| event.event_type != "Created")
                        .map(|event| event.event_type)
                        .collect();
                    assert_eq!(
                        actions,
                        if action == "TryArchiveThenInit" {
                            vec!["Init", "Touch"]
                        } else if child_status == "Archived" {
                            vec!["Init", "Archive"]
                        } else {
                            vec!["Init"]
                        },
                        "seed {seed}, {id}"
                    );
                } else {
                    assert!(
                        sim.dump_journal(&persistence_id)
                            .iter()
                            .all(|event| event.event_type == "Created"),
                        "seed {seed}, {id}: refused parent cannot initialize a child"
                    );
                    if existing {
                        let child = state
                            .get_tenant_entity_state(&TenantId::default(), "Child", &id)
                            .await
                            .unwrap();
                        assert_eq!(child.state.status, "New", "seed {seed}, {id}");
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
    }
}
