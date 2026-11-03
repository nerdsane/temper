//! A timer no one can run stops, its record says so, and a person's move runs it.

use super::*;

/// A timer that never fires, since its action binds a caller and nothing names
/// one when it is found with no row. Its record says so, once, and a person's
/// move runs it.
#[test]
fn a_timer_waiting_for_a_caller_says_so() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let start_at = sim.draw(500);
        let first = sim.start(3600);
        first.run(async {
            wait(start_at).await;
            let tenant = TenantId::default();
            first
                .state
                .get_or_create_tenant_entity(&tenant, "Shipment", "s-1", json!({}))
                .await
                .expect("the record is created");
            let started = first
                .state
                .dispatch_tenant_action(
                    &tenant,
                    "Shipment",
                    "s-1",
                    "Start",
                    json!({"parent_type": "", "parent_id": "", "line_id": ""}),
                    &AgentContext::system(),
                )
                .await
                .expect("the move is dispatched");
            assert!(started.success, "{trace}: {:?}", started.error);
            settle().await;
        });
        sim.stop(first);
        let rows = sim.store.list_state_timers("default");
        let row = rows
            .iter()
            .find(|row| row.entity_id == "s-1")
            .expect("the timer left a row");
        sim.store
            .delete_state_timer("default", "Shipment", "s-1", &row.arm_id);

        let second = sim.start(1);
        let record = second.run(until(
            &second.state,
            "Shipments",
            "s-1",
            "said it needs a person",
            is_stopped,
        ));
        assert_eq!(
            record["timer_needs_person"], "Diagnose",
            "{trace}: {record}"
        );
        assert_eq!(record["status"], "Staging", "{trace}");
        sim.stop(second);
        let third = sim.start(1);
        third.run(wait(Duration::from_secs(2)));
        assert_eq!(
            sim.logs.stops("s-1"),
            1,
            "{trace}: a record already saying so says it once"
        );

        third.run(async {
            act(&third.state, "Shipment", "s-1", "Nudge", &person("gil")).await;
            let record = until(&third.state, "Shipments", "s-1", "fired", |record| {
                record["status"] == "Diagnosed"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "gil", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// A chain no one started: the timed action that binds its caller fires once
/// and is refused. It does not fire again, even after a restart. The record
/// says so until a person's move, whose caller its timer then fires as.
#[test]
fn a_timed_action_no_one_can_make_stops_until_a_person_moves() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let first = sim.start(1);
        first.run(async {
            wait(begin_at).await;
            act(
                &first.state,
                "Check",
                "c-1",
                "Begin",
                &AgentContext::system(),
            )
            .await;
            until(
                &first.state,
                "Checks",
                "c-1",
                "said it needs a person",
                is_stopped,
            )
            .await;
            wait(Duration::from_secs(3)).await;
        });
        sim.stop(first);
        assert_eq!(
            sim.logs.stops("c-1"),
            1,
            "{trace}: it fired once and was refused once"
        );
        assert!(
            sim.store
                .list_state_timers("default")
                .iter()
                .all(|row| row.entity_id != "c-1"),
            "{trace}: the stopped timer keeps no row"
        );

        let after = sim.start(1);
        after.run(async {
            let state = &after.state;
            wait(Duration::from_secs(3)).await;
            let record = read(state, "Checks", "c-1").await;
            assert_eq!(record["timer_needs_person"], "Finish", "{trace}: {record}");
            assert_eq!(record["status"], "Checking", "{trace}");
            assert_eq!(
                sim.logs.stops("c-1"),
                1,
                "{trace}: a restart does not fire it again"
            );

            act(state, "Check", "c-1", "Nudge", &person("cal")).await;
            let record = until(state, "Checks", "c-1", "finished", |record| {
                record["status"] == "Done"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "cal", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// The line's Rollout: the timed move lands and a reaction to it is refused.
/// On a timer that keeps resetting itself, that is a refusal on every fire.
#[test]
fn a_reaction_no_one_can_make_stops_a_repeating_timer() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let first = sim.start(1);
        first.run(async {
            wait(begin_at).await;
            act(
                &first.state,
                "Poller",
                "p-1",
                "Begin",
                &AgentContext::system(),
            )
            .await;
            until(
                &first.state,
                "Pollers",
                "p-1",
                "said it needs a person",
                is_stopped,
            )
            .await;
            wait(Duration::from_secs(3)).await;
        });
        sim.stop(first);
        let after = sim.start(1);
        after.run(async {
            let state = &after.state;
            wait(Duration::from_secs(3)).await;
            let record = read(state, "Pollers", "p-1").await;
            assert_eq!(
                record["fields"]["polls"], 1,
                "{trace}: one fire, restart or not: {record}"
            );
            assert_eq!(
                record["timer_needs_person"], "Diagnose",
                "{trace}: {record}"
            );
            assert_eq!(sim.logs.stops("p-1"), 1, "{trace}");

            act(state, "Poller", "p-1", "Nudge", &person("dee")).await;
            let record = until(state, "Pollers", "p-1", "got stuck", |record| {
                record["status"] == "Stuck"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "dee", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// The same when the refusal comes from an integration's answer: the timer's
/// fire has returned and its next arm is set, and it is still the one stopped.
#[test]
fn a_refusal_after_an_integrations_answer_stops_the_timer_that_asked() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let server = sim.start(1);
        server.run(async {
            let state = &server.state;
            wait(begin_at).await;
            act(state, "Asker", "a-1", "Begin", &AgentContext::system()).await;
            until(state, "Askers", "a-1", "said it needs a person", is_stopped).await;
            wait(Duration::from_secs(3)).await;
            let record = read(state, "Askers", "a-1").await;
            assert_eq!(
                record["timer_needs_person"], "Diagnose",
                "{trace}: {record}"
            );
            assert!(sim.logs.stops("a-1") >= 1, "{trace}");
            wait(Duration::from_secs(3)).await;
            let later = read(state, "Askers", "a-1").await;
            assert_eq!(
                later["fields"]["polls"], record["fields"]["polls"],
                "{trace}: it stopped polling: {later}"
            );
        });
    });
}

/// A refusal whose timer is gone stops nothing: the timer that asked fired
/// once and the record moved on to a state whose own timer needs no caller.
/// That timer keeps running, and the record never says it needs anyone.
#[test]
fn a_late_refusal_does_not_stop_the_next_states_timer() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let server = sim.start(1);
        server.run(async {
            let state = &server.state;
            wait(begin_at).await;
            act(state, "Hopper", "h-1", "Begin", &AgentContext::system()).await;
            until(state, "Hoppers", "h-1", "heard its answer", |record| {
                record["fields"]["seen"] != ""
            })
            .await;
            let record = until(state, "Hoppers", "h-1", "kept ticking", |record| {
                record["fields"]["ticks"].as_i64().unwrap_or(0) >= 3
            })
            .await;
            assert_eq!(record["status"], "Waiting", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// A person's move finds a timer armed for no one and keeps it, as it keeps
/// any live timer. Its refusal then finds the person in the journal: it runs
/// as them, and the record never says it needs anyone.
#[test]
fn a_refusal_after_a_persons_move_runs_as_that_person() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let server = sim.start(1);
        server.run(async {
            let state = &server.state;
            wait(begin_at).await;
            act(state, "Check", "c-2", "Begin", &AgentContext::system()).await;
            act(state, "Check", "c-2", "Nudge", &person("fay")).await;
            let record = until(state, "Checks", "c-2", "finished", |record| {
                record["status"] == "Done"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "fay", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// The same, when the journal can't be read at the refusal: a read that
/// failed is not proof that no one moved the record. The decision waits for
/// the journal, which names the person.
#[test]
fn a_journal_that_cannot_be_read_does_not_stop_the_timer() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let server = sim.start(1);
        server.run(async {
            let state = &server.state;
            wait(begin_at).await;
            act(state, "Check", "c-3", "Begin", &AgentContext::system()).await;
            act(state, "Check", "c-3", "Nudge", &person("hal")).await;
            // The record's actor is live, so the stop reads only the journal.
            sim.store.fail_next_reads("default:Check:c-3", 2);
            let record = until(state, "Checks", "c-3", "finished", |record| {
                record["status"] == "Done"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "hal", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}

/// A person's chain never stops: this is the same Poller, started by a person.
#[test]
fn a_chain_a_person_started_is_not_stopped() {
    Sim::each_seed(|sim| {
        let trace = format!("seed {}", sim.seed);
        let begin_at = sim.draw(500);
        let server = sim.start(1);
        server.run(async {
            let state = &server.state;
            wait(begin_at).await;
            act(state, "Poller", "p-2", "Begin", &person("eve")).await;
            let record = until(state, "Pollers", "p-2", "got stuck", |record| {
                record["status"] == "Stuck"
            })
            .await;
            assert_eq!(record["fields"]["fired_by"], "eve", "{trace}");
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "{trace}: {record}"
            );
        });
    });
}
