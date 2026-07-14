use std::collections::BTreeSet;

use super::*;

fn drain_until_quiescent(sched: &mut SimScheduler, max_ticks: u64) -> u64 {
    for _ in 0..max_ticks {
        if sched.is_quiescent() {
            break;
        }
        sched.tick();
        sched.drain_ready(1_024);
    }
    sched.ticks
}

#[test]
fn test_basic_message_delivery() {
    let mut sched = SimScheduler::new(1, FaultConfig::none());
    sched.register_actor("actor-a");
    sched.register_actor("actor-b");

    sched.send("actor-a", "actor-b", "Ping", "{}");
    assert_eq!(sched.total_delivered(), 0);

    sched.tick(); // deliver
    assert_eq!(sched.total_delivered(), 1);

    let msg = sched.drain_ready(1).pop().unwrap();
    assert_eq!(msg.msg_type, "Ping");
    assert_eq!(msg.from, "actor-a");
    assert!(sched.drain_ready(1).is_empty());
}

#[test]
fn test_message_ordering_is_deterministic() {
    // Run the same scenario twice with the same seed → same delivery order
    fn run_scenario(seed: u64) -> Vec<String> {
        let mut sched = SimScheduler::new(seed, FaultConfig::light());
        sched.register_actor("a");
        sched.register_actor("b");

        for i in 0..10 {
            sched.send("a", "b", &format!("msg-{i}"), "{}");
        }

        drain_until_quiescent(&mut sched, 100);

        sched
            .delivered_log()
            .iter()
            .map(|m| m.msg_type.clone())
            .collect()
    }

    let run1 = run_scenario(42);
    let run2 = run_scenario(42);
    assert_eq!(run1, run2, "Same seed must produce same delivery order");
}

#[test]
fn test_different_seeds_may_produce_different_order() {
    fn run_scenario(seed: u64) -> Vec<String> {
        let mut sched = SimScheduler::new(seed, FaultConfig::light());
        sched.register_actor("a");
        sched.register_actor("b");

        for i in 0..20 {
            sched.send("a", "b", &format!("msg-{i}"), "{}");
        }

        drain_until_quiescent(&mut sched, 100);
        sched
            .delivered_log()
            .iter()
            .map(|m| m.msg_type.clone())
            .collect()
    }

    let run1 = run_scenario(42);
    let run2 = run_scenario(999);
    // With light faults (10% delay), different seeds should likely produce different orders
    // This isn't guaranteed for every pair, but is overwhelmingly likely with 20 messages
    assert_ne!(
        run1, run2,
        "Different seeds should usually produce different orders"
    );
}

#[test]
fn test_fault_injection_message_drop() {
    let config = FaultConfig {
        message_drop_prob: 1.0, // Drop everything
        ..FaultConfig::none()
    };
    let mut sched = SimScheduler::new(42, config);
    sched.register_actor("a");
    sched.register_actor("b");

    sched.send("a", "b", "Important", "{}");
    sched.tick();

    assert_eq!(sched.total_delivered(), 0);
    assert_eq!(sched.total_dropped(), 1);
}

#[test]
fn test_fault_injection_actor_crash() {
    let config = FaultConfig {
        actor_crash_prob: 1.0, // Crash after every tick
        ..FaultConfig::none()
    };
    let mut sched = SimScheduler::new(42, config);
    sched.register_actor("a");
    sched.register_actor("b");

    sched.send("a", "b", "msg", "{}");
    sched.tick();

    // Message should be delivered (crash happens AFTER delivery)
    assert_eq!(sched.total_delivered(), 1);

    // But one of the actors should now be crashed
    let crashed = sched
        .actor_states
        .values()
        .filter(|s| **s == SimActorState::Crashed)
        .count();
    assert!(crashed > 0, "Should have at least one crashed actor");
}

#[test]
fn test_message_to_crashed_actor_is_dropped() {
    let mut sched = SimScheduler::new(42, FaultConfig::none());
    sched.register_actor("a");
    sched.register_actor("b");

    // Manually crash actor-b
    sched
        .actor_states
        .insert("b".to_string(), SimActorState::Crashed);

    sched.send("a", "b", "msg", "{}");
    sched.tick();

    assert_eq!(sched.total_delivered(), 0);
    assert_eq!(sched.total_dropped(), 1);
}

#[test]
fn test_quiescence_detection() {
    let mut sched = SimScheduler::new(1, FaultConfig::none());
    sched.register_actor("a");

    assert!(sched.is_quiescent());

    sched.send("a", "a", "self-msg", "{}");
    assert!(!sched.is_quiescent());

    sched.tick();
    // Message delivered to mailbox — not quiescent until consumed
    sched.drain_ready(1);
    assert!(sched.is_quiescent());
}

#[test]
fn test_budgeted_drain_preserves_ready_messages() {
    let mut sched = SimScheduler::new(1, FaultConfig::none());
    sched.register_actor("a");
    sched.register_actor("b");

    sched.send("a", "b", "msg-1", "{}");
    sched.send("a", "b", "msg-2", "{}");
    sched.send("a", "b", "msg-3", "{}");

    sched.tick();
    let first = sched.drain_ready(2);
    assert_eq!(first.len(), 2);
    assert_eq!(sched.mailbox_depth("b"), 1);

    let second = sched.drain_ready(2);
    assert_eq!(second.len(), 1);
    assert_eq!(sched.mailbox_depth("b"), 0);
    assert!(sched.drain_ready(2).is_empty());
    assert_eq!(sched.total_delivered(), 3);
}

#[test]
#[should_panic(expected = "ready mailbox budget exhausted for actor 'b'")]
fn test_ready_mailbox_budget_fails_fast() {
    let mut sched = SimScheduler::with_mailbox_budget(1, FaultConfig::none(), 1);
    sched.register_actor("a");
    sched.register_actor("b");
    sched.send("a", "b", "first", "{}");
    sched.send("a", "b", "second", "{}");
    sched.tick();
}

#[test]
fn test_drain_ready_transfers_ownership_once_in_actor_order() {
    let mut sched = SimScheduler::new(1, FaultConfig::none());
    sched.register_actor("actor-b");
    sched.register_actor("actor-a");

    sched.send("driver", "actor-b", "ForB", "{}");
    sched.send("driver", "actor-a", "ForA", "{}");
    sched.send("driver", "actor-b", "ForB2", "{}");
    sched.send("driver", "actor-a", "ForA2", "{}");
    sched.tick();

    let owners: Vec<String> = (0..4)
        .map(|_| sched.drain_ready(1).pop().unwrap().to)
        .collect();
    assert_eq!(owners, vec!["actor-a", "actor-b", "actor-a", "actor-b"]);
    assert!(sched.drain_ready(2).is_empty());
    assert!(sched.is_quiescent());
}

#[test]
fn test_delayed_delivery_is_exactly_once_across_replay_seeds() {
    for seed in 0..16 {
        let mut sched = SimScheduler::new(
            seed,
            FaultConfig {
                message_delay_prob: 1.0,
                max_delay_ticks: 5,
                ..FaultConfig::none()
            },
        );
        sched.register_actor("actor-a");
        sched.register_actor("actor-b");
        for id in 0..32 {
            let actor = if id % 2 == 0 { "actor-a" } else { "actor-b" };
            sched.send("driver", actor, "Apply", "{}");
        }

        let mut received_ids = BTreeSet::new();
        for _ in 0..16 {
            sched.tick();
            for message in sched.drain_ready(3) {
                assert!(
                    received_ids.insert(message.id),
                    "seed {seed} returned message {} twice",
                    message.id
                );
            }
        }
        while !sched.is_quiescent() {
            for message in sched.drain_ready(3) {
                assert!(received_ids.insert(message.id));
            }
        }

        assert_eq!(received_ids.len(), 32, "seed {seed} lost a delivery");
        assert_eq!(sched.total_delivered(), 32);
        assert_eq!(sched.total_dropped(), 0);
    }
}

#[test]
fn test_message_delay_increases_delivery_time() {
    let config = FaultConfig {
        message_delay_prob: 1.0, // Always delay
        max_delay_ticks: 5,
        ..FaultConfig::none()
    };
    let mut sched = SimScheduler::new(42, config);
    sched.register_actor("a");
    sched.register_actor("b");

    sched.send("a", "b", "delayed", "{}");

    // Tick 1: message not yet delivered (delayed)
    sched.tick();
    let delivered_at_1 = sched.total_delivered();

    // Run more ticks
    drain_until_quiescent(&mut sched, 20);
    assert_eq!(
        sched.total_delivered(),
        1,
        "Message should eventually arrive"
    );
    if delivered_at_1 == 0 {
        assert!(
            sched.current_time() > 1,
            "Delivery should be delayed beyond tick 1"
        );
    }
}

#[test]
fn test_heavy_faults_simulation_completes() {
    // Even with heavy faults, simulation should complete without panic
    let mut sched = SimScheduler::new(12345, FaultConfig::heavy());
    for i in 0..5 {
        sched.register_actor(&format!("actor-{i}"));
    }

    // Send 50 messages between random actors
    let mut rng = super::super::DeterministicRng::new(67890);
    for _ in 0..50 {
        let from = format!("actor-{}", rng.next_bound(5));
        let to = format!("actor-{}", rng.next_bound(5));
        sched.send(&from, &to, "msg", "{}");
    }

    drain_until_quiescent(&mut sched, 200);

    // Just verify it completed without panic and some messages got through
    let total = sched.total_delivered() + sched.total_dropped();
    assert!(total > 0, "Should have processed some messages");
}

#[test]
fn test_send_at_delivers_at_specified_time() {
    let mut sched = SimScheduler::new(1, FaultConfig::none());
    sched.register_actor("a");
    sched.register_actor("b");

    // Schedule a message at time 5
    sched.send_at("a", "b", "Scheduled", "{}", 5);

    // Ticks 1-4: nothing delivered
    for _ in 1..5 {
        sched.tick();
        assert_eq!(
            sched.total_delivered(),
            0,
            "should not deliver before deliver_at"
        );
    }

    // Tick 5: message delivered
    sched.tick();
    assert_eq!(sched.total_delivered(), 1);

    let msg = sched.drain_ready(1).pop().unwrap();
    assert_eq!(msg.msg_type, "Scheduled");
    assert_eq!(msg.deliver_at, 5);
}

#[test]
fn test_send_at_respects_message_drop() {
    let config = FaultConfig {
        message_drop_prob: 1.0,
        ..FaultConfig::none()
    };
    let mut sched = SimScheduler::new(42, config);
    sched.register_actor("a");
    sched.register_actor("b");

    sched.send_at("a", "b", "Scheduled", "{}", 3);
    drain_until_quiescent(&mut sched, 10);

    assert_eq!(sched.total_delivered(), 0);
    assert_eq!(sched.total_dropped(), 1);
}
