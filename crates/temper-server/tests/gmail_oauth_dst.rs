//! Deterministic Simulation Tests for Gmail OAuth2 flow.
//!
//! Exercises all three new features together:
//! - **Secret templates**: `{secret:key}` in integration configs
//! - **Webhooks**: `[[webhook]]` section for inbound OAuth callbacks
//! - **Scheduled actions**: `Schedule` effect for token refresh timers
//!
//! The spec models a complete OAuth2 lifecycle:
//! Disconnected → AwaitingAuth → Exchanging → Authenticated ⇄ Refreshing → Expired

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const GMAIL_OAUTH_IOA: &str = include_str!("../../../test-fixtures/specs/gmail_oauth.ioa.toml");

fn gmail_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(GMAIL_OAUTH_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — OAuth2 Lifecycle
// =========================================================================

#[test]
fn starts_in_disconnected() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    sim.assert_status("auth-1", "Disconnected");
}

#[test]
fn full_oauth_lifecycle() {
    let config = SimActorSystemConfig {
        seed: 2,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    // 1. StartOAuth: Disconnected → AwaitingAuth
    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.assert_status("auth-1", "AwaitingAuth");

    // 2. OAuthCallback (simulated webhook): AwaitingAuth → Exchanging
    sim.step("auth-1", "OAuthCallback", r#"{"code":"abc123"}"#)
        .unwrap();
    sim.assert_status("auth-1", "Exchanging");

    // 3. ExchangeSucceeded (simulated WASM callback): Exchanging → Authenticated
    //    This also schedules a RefreshToken timer via Schedule effect.
    sim.step(
        "auth-1",
        "ExchangeSucceeded",
        r#"{"access_token":"at_xxx","refresh_token":"rt_xxx"}"#,
    )
    .unwrap();
    sim.assert_status("auth-1", "Authenticated");

    // 4. RefreshToken (simulated timer fire): Authenticated → Refreshing
    sim.step("auth-1", "RefreshToken", "{}").unwrap();
    sim.assert_status("auth-1", "Refreshing");

    // 5. RefreshSucceeded: Refreshing → Authenticated (re-schedules timer)
    sim.step("auth-1", "RefreshSucceeded", "{}").unwrap();
    sim.assert_status("auth-1", "Authenticated");

    sim.assert_event_count("auth-1", 5);
    assert!(!sim.has_violations(), "violations: {:?}", sim.violations());
}

#[test]
fn exchange_failure_returns_to_disconnected() {
    let config = SimActorSystemConfig {
        seed: 3,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.step("auth-1", "OAuthCallback", r#"{"code":"bad"}"#)
        .unwrap();
    sim.assert_status("auth-1", "Exchanging");

    // Exchange fails → back to Disconnected (can retry)
    sim.step("auth-1", "ExchangeFailed", "{}").unwrap();
    sim.assert_status("auth-1", "Disconnected");

    // Retry: start OAuth again from Disconnected
    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.assert_status("auth-1", "AwaitingAuth");

    assert!(!sim.has_violations());
}

#[test]
fn refresh_failure_leads_to_expired() {
    let config = SimActorSystemConfig {
        seed: 4,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    // Get to Authenticated
    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.step("auth-1", "OAuthCallback", r#"{"code":"x"}"#)
        .unwrap();
    sim.step(
        "auth-1",
        "ExchangeSucceeded",
        r#"{"access_token":"a","refresh_token":"r"}"#,
    )
    .unwrap();
    sim.assert_status("auth-1", "Authenticated");

    // Refresh attempt fails
    sim.step("auth-1", "RefreshToken", "{}").unwrap();
    sim.step("auth-1", "RefreshFailed", "{}").unwrap();
    sim.assert_status("auth-1", "Expired");

    // Recovery: StartOAuth from Expired → AwaitingAuth
    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.assert_status("auth-1", "AwaitingAuth");

    assert!(!sim.has_violations());
}

#[test]
fn multiple_refresh_cycles() {
    let config = SimActorSystemConfig {
        seed: 5,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    // Initial auth
    sim.step("auth-1", "StartOAuth", "{}").unwrap();
    sim.step("auth-1", "OAuthCallback", r#"{"code":"c"}"#)
        .unwrap();
    sim.step(
        "auth-1",
        "ExchangeSucceeded",
        r#"{"access_token":"a","refresh_token":"r"}"#,
    )
    .unwrap();

    // Simulate 5 refresh cycles (the self-recursive timer pattern)
    for _ in 0..5 {
        sim.step("auth-1", "RefreshToken", "{}").unwrap();
        sim.assert_status("auth-1", "Refreshing");
        sim.step("auth-1", "RefreshSucceeded", "{}").unwrap();
        sim.assert_status("auth-1", "Authenticated");
    }

    // 3 initial + 10 refresh cycles = 13 events
    sim.assert_event_count("auth-1", 13);
    assert!(!sim.has_violations());
}

#[test]
fn invalid_transitions_rejected() {
    let config = SimActorSystemConfig {
        seed: 6,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("GmailAuth", "auth-1", gmail_table())
        .with_ioa_invariants(GMAIL_OAUTH_IOA);
    sim.register_actor("auth-1", Box::new(handler));

    // Can't callback without starting OAuth
    let result = sim.step("auth-1", "OAuthCallback", r#"{"code":"x"}"#);
    assert!(
        result.is_err(),
        "OAuthCallback should fail from Disconnected"
    );

    // Can't exchange without callback
    let result = sim.step("auth-1", "ExchangeSucceeded", "{}");
    assert!(
        result.is_err(),
        "ExchangeSucceeded should fail from Disconnected"
    );

    // Can't refresh without being authenticated
    let result = sim.step("auth-1", "RefreshToken", "{}");
    assert!(
        result.is_err(),
        "RefreshToken should fail from Disconnected"
    );

    sim.assert_status("auth-1", "Disconnected");
    assert!(!sim.has_violations());
}

// =========================================================================
// RANDOM EXPLORATION — Fault injection
// =========================================================================

#[test]
fn random_no_faults() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let id = format!("auth-{i}");
        let handler = EntityActorHandler::new("GmailAuth", id.clone(), gmail_table())
            .with_ioa_invariants(GMAIL_OAUTH_IOA);
        sim.register_actor(&id, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Random no-fault exploration broke invariants: {:?}",
        result.violations
    );
    assert!(result.transitions > 0);
}

#[test]
fn random_light_faults() {
    let config = SimActorSystemConfig {
        seed: 99,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let id = format!("auth-{i}");
        let handler = EntityActorHandler::new("GmailAuth", id.clone(), gmail_table())
            .with_ioa_invariants(GMAIL_OAUTH_IOA);
        sim.register_actor(&id, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults broke invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 1337,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let id = format!("auth-{i}");
        let handler = EntityActorHandler::new("GmailAuth", id.clone(), gmail_table())
            .with_ioa_invariants(GMAIL_OAUTH_IOA);
        sim.register_actor(&id, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Heavy faults broke invariants: {:?}",
        result.violations
    );
}

// =========================================================================
// DETERMINISM PROOFS — same seed = bit-exact same outcome
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let id = format!("auth-{i}");
        let handler = EntityActorHandler::new("GmailAuth", id.clone(), gmail_table())
            .with_ioa_invariants(GMAIL_OAUTH_IOA);
        sim.register_actor(&id, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(result.all_invariants_held);
    result.actor_states
}

#[test]
fn determinism_proof_seed_42() {
    let reference = run_determinism_trial(42);
    for run in 1..10 {
        let trial = run_determinism_trial(42);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 42"
        );
    }
}

#[test]
fn determinism_proof_seed_1337() {
    let reference = run_determinism_trial(1337);
    for run in 1..10 {
        let trial = run_determinism_trial(1337);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 1337"
        );
    }
}

// =========================================================================
// MULTI-SEED SWEEP — bulk exploration
// =========================================================================

#[test]
fn multi_seed_sweep() {
    for seed in 0..20 {
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 100,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        let handler = EntityActorHandler::new("GmailAuth", "auth", gmail_table())
            .with_ioa_invariants(GMAIL_OAUTH_IOA);
        sim.register_actor("auth", Box::new(handler));

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Seed {seed} broke invariants: {:?}",
            result.violations
        );
    }
}
