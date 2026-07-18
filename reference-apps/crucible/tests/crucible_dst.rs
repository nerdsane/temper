//! Deterministic Simulation Tests for Crucible entities.
//!
//! These DST tests exercise the `Environment`, `EnvironmentAllowedHost`,
//! and `EnvironmentPackage` specs through `SimActorSystem` with:
//!
//! - **Scripted scenarios**: exact action sequences with state assertions.
//! - **Randomized exploration**: seed-controlled random walks with light faults.
//! - **Determinism proof**: two consecutive runs under the same seed must
//!   produce identical final state.
//!
//! Child entity specs have a single state and no transitions, so only the
//! `Environment` spec carries scripted lifecycle tests. The child specs are
//! exercised by the random walk (registration + zero-transition steady-state).

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const ENVIRONMENT_IOA: &str = include_str!("../specs/environment.ioa.toml");
const ALLOWED_HOST_IOA: &str = include_str!("../specs/environment_allowed_host.ioa.toml");
const PACKAGE_IOA: &str = include_str!("../specs/environment_package.ioa.toml");
const MANAGED_AGENT_IOA: &str = include_str!("../specs/managed_agent.ioa.toml");
const AGENT_MCP_SERVER_IOA: &str = include_str!("../specs/agent_mcp_server.ioa.toml");
const AGENT_SKILL_IOA: &str = include_str!("../specs/agent_skill.ioa.toml");
const AGENT_TOOL_IOA: &str = include_str!("../specs/agent_tool.ioa.toml");
const AGENT_TOOL_CONFIG_IOA: &str = include_str!("../specs/agent_tool_config.ioa.toml");
const AGENT_VERSION_IOA: &str = include_str!("../specs/agent_version.ioa.toml");
const SESSION_IOA: &str = include_str!("../specs/session.ioa.toml");
const SESSION_RESOURCE_IOA: &str = include_str!("../specs/session_resource.ioa.toml");
const SESSION_EVENT_IOA: &str = include_str!("../specs/session_event.ioa.toml");
const CALLABLE_AGENT_IOA: &str = include_str!("../specs/callable_agent.ioa.toml");
const SESSION_THREAD_IOA: &str = include_str!("../specs/session_thread.ioa.toml");

fn environment_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ENVIRONMENT_IOA))
}

fn allowed_host_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ALLOWED_HOST_IOA))
}

fn package_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PACKAGE_IOA))
}

fn managed_agent_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(MANAGED_AGENT_IOA))
}

fn agent_mcp_server_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(AGENT_MCP_SERVER_IOA))
}

fn agent_skill_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(AGENT_SKILL_IOA))
}

fn agent_tool_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(AGENT_TOOL_IOA))
}

fn agent_tool_config_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(AGENT_TOOL_CONFIG_IOA))
}

fn agent_version_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(AGENT_VERSION_IOA))
}

fn session_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(SESSION_IOA))
}

fn session_resource_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(SESSION_RESOURCE_IOA))
}

fn session_event_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(SESSION_EVENT_IOA))
}

fn callable_agent_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(CALLABLE_AGENT_IOA))
}

fn session_thread_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(SESSION_THREAD_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — Environment Lifecycle
// =========================================================================

#[test]
fn scripted_env_starts_active() {
    let config = SimActorSystemConfig {
        seed: 1,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.assert_status("env-1", "Active");
}

#[test]
fn scripted_env_archive_transitions_to_archived() {
    let config = SimActorSystemConfig {
        seed: 2,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.step("env-1", "ArchiveEnvironment", "{}").unwrap();
    sim.assert_status("env-1", "Archived");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_env_archive_is_final() {
    let config = SimActorSystemConfig {
        seed: 3,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Environment", "env-1", environment_table())
        .with_ioa_invariants(ENVIRONMENT_IOA);
    sim.register_actor("env-1", Box::new(handler));

    sim.step("env-1", "ArchiveEnvironment", "{}").unwrap();
    sim.assert_status("env-1", "Archived");

    // Second archive must be rejected — Archived is terminal.
    let result = sim.step("env-1", "ArchiveEnvironment", "{}");
    assert!(
        result.is_err(),
        "ArchiveEnvironment should fail from Archived state"
    );

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Child entities start Active
// =========================================================================

#[test]
fn scripted_allowed_host_starts_active() {
    let config = SimActorSystemConfig {
        seed: 4,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("EnvironmentAllowedHost", "host-1", allowed_host_table())
        .with_ioa_invariants(ALLOWED_HOST_IOA);
    sim.register_actor("host-1", Box::new(handler));

    sim.assert_status("host-1", "Active");
}

#[test]
fn scripted_package_starts_active() {
    let config = SimActorSystemConfig {
        seed: 5,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("EnvironmentPackage", "pkg-1", package_table())
        .with_ioa_invariants(PACKAGE_IOA);
    sim.register_actor("pkg-1", Box::new(handler));

    sim.assert_status("pkg-1", "Active");
}

// =========================================================================
// SCRIPTED SCENARIOS — ManagedAgent Lifecycle (ADR-0043)
// =========================================================================

#[test]
fn scripted_managed_agent_starts_active() {
    let config = SimActorSystemConfig {
        seed: 10,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("ManagedAgent", "agent-1", managed_agent_table())
        .with_ioa_invariants(MANAGED_AGENT_IOA);
    sim.register_actor("agent-1", Box::new(handler));

    sim.assert_status("agent-1", "Active");
}

#[test]
fn scripted_managed_agent_archive_transitions_to_archived() {
    let config = SimActorSystemConfig {
        seed: 11,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("ManagedAgent", "agent-1", managed_agent_table())
        .with_ioa_invariants(MANAGED_AGENT_IOA);
    sim.register_actor("agent-1", Box::new(handler));

    sim.step("agent-1", "ArchiveManagedAgent", "{}").unwrap();
    sim.assert_status("agent-1", "Archived");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_managed_agent_archive_is_final() {
    let config = SimActorSystemConfig {
        seed: 12,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("ManagedAgent", "agent-1", managed_agent_table())
        .with_ioa_invariants(MANAGED_AGENT_IOA);
    sim.register_actor("agent-1", Box::new(handler));

    sim.step("agent-1", "ArchiveManagedAgent", "{}").unwrap();
    sim.assert_status("agent-1", "Archived");

    // Second archive must be rejected — Archived is terminal.
    let result = sim.step("agent-1", "ArchiveManagedAgent", "{}");
    assert!(
        result.is_err(),
        "ArchiveManagedAgent should fail from Archived state"
    );

    assert!(!sim.has_violations());
}

#[test]
fn scripted_managed_agent_child_entities_start_active() {
    let config = SimActorSystemConfig {
        seed: 13,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        (
            "mcp-1",
            "AgentMcpServer",
            agent_mcp_server_table(),
            AGENT_MCP_SERVER_IOA,
        ),
        (
            "skill-1",
            "AgentSkill",
            agent_skill_table(),
            AGENT_SKILL_IOA,
        ),
        ("tool-1", "AgentTool", agent_tool_table(), AGENT_TOOL_IOA),
        (
            "config-1",
            "AgentToolConfig",
            agent_tool_config_table(),
            AGENT_TOOL_CONFIG_IOA,
        ),
        (
            "version-1",
            "AgentVersion",
            agent_version_table(),
            AGENT_VERSION_IOA,
        ),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
        sim.assert_status(name, "Active");
    }
}

// =========================================================================
// SCRIPTED SCENARIOS — Session Lifecycle (ADR-0044)
// =========================================================================

#[test]
fn scripted_session_starts_rescheduling() {
    let config = SimActorSystemConfig {
        seed: 20,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.assert_status("sess-1", "Rescheduling");
}

#[test]
fn scripted_session_full_lifecycle() {
    // Rescheduling → Running → Idle → Running → Terminated → Archived
    let config = SimActorSystemConfig {
        seed: 21,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.assert_status("sess-1", "Running");

    sim.step("sess-1", "IdleSession", "{}").unwrap();
    sim.assert_status("sess-1", "Idle");

    sim.step("sess-1", "ResumeSession", "{}").unwrap();
    sim.assert_status("sess-1", "Running");

    sim.step("sess-1", "TerminateSession", "{}").unwrap();
    sim.assert_status("sess-1", "Terminated");

    sim.step("sess-1", "ArchiveSession", "{}").unwrap();
    sim.assert_status("sess-1", "Archived");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_reschedule_roundtrip() {
    // Rescheduling → Running → Rescheduling → Running
    let config = SimActorSystemConfig {
        seed: 22,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.assert_status("sess-1", "Running");

    sim.step("sess-1", "RescheduleSession", "{}").unwrap();
    sim.assert_status("sess-1", "Rescheduling");

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.assert_status("sess-1", "Running");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_terminate_from_idle() {
    // Rescheduling → Running → Idle → Terminated (multi-from test)
    let config = SimActorSystemConfig {
        seed: 23,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.step("sess-1", "IdleSession", "{}").unwrap();
    sim.step("sess-1", "TerminateSession", "{}").unwrap();
    sim.assert_status("sess-1", "Terminated");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_archive_requires_terminated() {
    // ArchiveSession must fail from Running — only Terminated → Archived is valid
    let config = SimActorSystemConfig {
        seed: 24,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.assert_status("sess-1", "Running");

    let result = sim.step("sess-1", "ArchiveSession", "{}");
    assert!(
        result.is_err(),
        "ArchiveSession should fail from Running — must Terminate first"
    );

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_archive_is_final() {
    let config = SimActorSystemConfig {
        seed: 25,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Session", "sess-1", session_table())
        .with_ioa_invariants(SESSION_IOA);
    sim.register_actor("sess-1", Box::new(handler));

    sim.step("sess-1", "StartSession", "{}").unwrap();
    sim.step("sess-1", "TerminateSession", "{}").unwrap();
    sim.step("sess-1", "ArchiveSession", "{}").unwrap();
    sim.assert_status("sess-1", "Archived");

    // No further transitions allowed from Archived.
    for action in [
        "StartSession",
        "IdleSession",
        "ResumeSession",
        "RescheduleSession",
        "TerminateSession",
        "ArchiveSession",
    ] {
        let result = sim.step("sess-1", action, "{}");
        assert!(
            result.is_err(),
            "{action} should fail from terminal Archived state"
        );
    }

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_child_entities_start_active() {
    let config = SimActorSystemConfig {
        seed: 26,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        (
            "resource-1",
            "SessionResource",
            session_resource_table(),
            SESSION_RESOURCE_IOA,
        ),
        (
            "event-1",
            "SessionEvent",
            session_event_table(),
            SESSION_EVENT_IOA,
        ),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
        sim.assert_status(name, "Active");
    }
}

// =========================================================================
// SCRIPTED SCENARIOS — Multi-Agent (CallableAgent, SessionThread)
// =========================================================================

#[test]
fn scripted_callable_agent_starts_active() {
    let config = SimActorSystemConfig {
        seed: 30,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("CallableAgent", "ca-1", callable_agent_table())
        .with_ioa_invariants(CALLABLE_AGENT_IOA);
    sim.register_actor("ca-1", Box::new(handler));

    sim.assert_status("ca-1", "Active");
}

#[test]
fn scripted_session_thread_full_lifecycle() {
    // Running → Idle → Running → Terminated
    let config = SimActorSystemConfig {
        seed: 31,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("SessionThread", "thread-1", session_thread_table())
        .with_ioa_invariants(SESSION_THREAD_IOA);
    sim.register_actor("thread-1", Box::new(handler));

    sim.assert_status("thread-1", "Running");

    sim.step("thread-1", "IdleThread", "{}").unwrap();
    sim.assert_status("thread-1", "Idle");

    sim.step("thread-1", "ResumeThread", "{}").unwrap();
    sim.assert_status("thread-1", "Running");

    sim.step("thread-1", "TerminateThread", "{}").unwrap();
    sim.assert_status("thread-1", "Terminated");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_session_thread_terminated_is_final() {
    let config = SimActorSystemConfig {
        seed: 32,
        ..Default::default()
    };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("SessionThread", "thread-1", session_thread_table())
        .with_ioa_invariants(SESSION_THREAD_IOA);
    sim.register_actor("thread-1", Box::new(handler));

    sim.step("thread-1", "TerminateThread", "{}").unwrap();
    sim.assert_status("thread-1", "Terminated");

    // No further transitions allowed from Terminated.
    for action in ["IdleThread", "ResumeThread", "TerminateThread"] {
        let result = sim.step("thread-1", action, "{}");
        assert!(
            result.is_err(),
            "{action} should fail from terminal Terminated state"
        );
    }

    assert!(!sim.has_violations());
}

// =========================================================================
// RANDOM EXPLORATION
// =========================================================================

#[test]
fn random_all_entities_no_faults() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 20,
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        ("env-1", "Environment", environment_table(), ENVIRONMENT_IOA),
        (
            "host-1",
            "EnvironmentAllowedHost",
            allowed_host_table(),
            ALLOWED_HOST_IOA,
        ),
        ("pkg-1", "EnvironmentPackage", package_table(), PACKAGE_IOA),
        (
            "agent-1",
            "ManagedAgent",
            managed_agent_table(),
            MANAGED_AGENT_IOA,
        ),
        (
            "mcp-1",
            "AgentMcpServer",
            agent_mcp_server_table(),
            AGENT_MCP_SERVER_IOA,
        ),
        (
            "skill-1",
            "AgentSkill",
            agent_skill_table(),
            AGENT_SKILL_IOA,
        ),
        ("tool-1", "AgentTool", agent_tool_table(), AGENT_TOOL_IOA),
        (
            "config-1",
            "AgentToolConfig",
            agent_tool_config_table(),
            AGENT_TOOL_CONFIG_IOA,
        ),
        (
            "version-1",
            "AgentVersion",
            agent_version_table(),
            AGENT_VERSION_IOA,
        ),
        ("sess-1", "Session", session_table(), SESSION_IOA),
        (
            "resource-1",
            "SessionResource",
            session_resource_table(),
            SESSION_RESOURCE_IOA,
        ),
        (
            "event-1",
            "SessionEvent",
            session_event_table(),
            SESSION_EVENT_IOA,
        ),
        (
            "callable-1",
            "CallableAgent",
            callable_agent_table(),
            CALLABLE_AGENT_IOA,
        ),
        (
            "thread-1",
            "SessionThread",
            session_thread_table(),
            SESSION_THREAD_IOA,
        ),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Random exploration found invariant violations: {:?}",
        result.violations
    );
}

// =========================================================================
// DETERMINISM PROOF — two runs under the same seed must match bit-exact
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 200,
        faults: FaultConfig::light(),
        max_actions_per_actor: 20,
    };
    let mut sim = SimActorSystem::new(config);

    for (name, entity_type, table, ioa) in [
        ("env-1", "Environment", environment_table(), ENVIRONMENT_IOA),
        (
            "host-1",
            "EnvironmentAllowedHost",
            allowed_host_table(),
            ALLOWED_HOST_IOA,
        ),
        ("pkg-1", "EnvironmentPackage", package_table(), PACKAGE_IOA),
        (
            "agent-1",
            "ManagedAgent",
            managed_agent_table(),
            MANAGED_AGENT_IOA,
        ),
        (
            "mcp-1",
            "AgentMcpServer",
            agent_mcp_server_table(),
            AGENT_MCP_SERVER_IOA,
        ),
        (
            "skill-1",
            "AgentSkill",
            agent_skill_table(),
            AGENT_SKILL_IOA,
        ),
        ("tool-1", "AgentTool", agent_tool_table(), AGENT_TOOL_IOA),
        (
            "config-1",
            "AgentToolConfig",
            agent_tool_config_table(),
            AGENT_TOOL_CONFIG_IOA,
        ),
        (
            "version-1",
            "AgentVersion",
            agent_version_table(),
            AGENT_VERSION_IOA,
        ),
        ("sess-1", "Session", session_table(), SESSION_IOA),
        (
            "resource-1",
            "SessionResource",
            session_resource_table(),
            SESSION_RESOURCE_IOA,
        ),
        (
            "event-1",
            "SessionEvent",
            session_event_table(),
            SESSION_EVENT_IOA,
        ),
        (
            "callable-1",
            "CallableAgent",
            callable_agent_table(),
            CALLABLE_AGENT_IOA,
        ),
        (
            "thread-1",
            "SessionThread",
            session_thread_table(),
            SESSION_THREAD_IOA,
        ),
    ] {
        let handler = EntityActorHandler::new(entity_type, name, table).with_ioa_invariants(ioa);
        sim.register_actor(name, Box::new(handler));
    }

    let result = sim.run_random();
    assert!(result.all_invariants_held);
    result.actor_states
}

#[test]
fn determinism_proof_seed_7() {
    let reference = run_determinism_trial(7);
    for run in 1..5 {
        let trial = run_determinism_trial(7);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 7 must produce identical results"
        );
    }
}

#[test]
fn determinism_proof_seed_1337() {
    let reference = run_determinism_trial(1337);
    for run in 1..5 {
        let trial = run_determinism_trial(1337);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 1337 must produce identical results"
        );
    }
}
