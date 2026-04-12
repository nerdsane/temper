//! Verification Cascade Tests for the Crucible reference app.
//!
//! Runs the full 3-level `VerificationCascade` against each of Crucible's
//! three IOA specs (`Environment`, `EnvironmentAllowedHost`,
//! `EnvironmentPackage`) and validates that the CSDL and cross-invariant
//! grammars parse cleanly.
//!
//! - L1 Model Check: Stateright exhaustive state exploration
//! - L2 Simulation: Deterministic simulation with fault injection
//! - L3 Property Test: Random action sequences with TLA-style invariants
//!
//! Additionally — as an L4-equivalent sanity pass — the cascade tests
//! validate that every field name referenced in the `[[field_invariant]]`
//! rules and the extended `[[cross_invariant]]` assertions resolves to a
//! property that actually exists in the CSDL. Without this cross-check
//! a typo in a field name would silently degrade a hard constraint into
//! a no-op.

use temper_spec::automaton::parse_automaton;
use temper_spec::cross_invariant::parse_cross_invariants;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::{CascadeLevel, VerificationCascade};

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
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

fn assert_cascade_passes(name: &str, ioa: &str) {
    let cascade = VerificationCascade::from_ioa(ioa)
        .with_sim_seeds(10)
        .with_prop_test_cases(500);

    let result = cascade.run();

    for level in &result.levels {
        assert!(
            level.passed,
            "{name} cascade level failed: {}",
            level.summary
        );
    }

    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .unwrap()
            .passed,
        "{name} L1 Model Check should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::Simulation)
            .unwrap()
            .passed,
        "{name} L2 Simulation should pass"
    );
    assert!(
        result
            .level_result(CascadeLevel::PropertyTest)
            .unwrap()
            .passed,
        "{name} L3 Property Tests should pass"
    );
    assert!(result.all_passed, "{name} cascade should pass all levels");
}

#[test]
fn cascade_environment_all_levels_pass() {
    assert_cascade_passes("Environment", ENVIRONMENT_IOA);
}

#[test]
fn cascade_environment_allowed_host_all_levels_pass() {
    assert_cascade_passes("EnvironmentAllowedHost", ALLOWED_HOST_IOA);
}

#[test]
fn cascade_environment_package_all_levels_pass() {
    assert_cascade_passes("EnvironmentPackage", PACKAGE_IOA);
}

// --- ManagedAgent slice (ADR-0043) -----------------------------------------

#[test]
fn cascade_managed_agent_all_levels_pass() {
    assert_cascade_passes("ManagedAgent", MANAGED_AGENT_IOA);
}

#[test]
fn cascade_agent_mcp_server_all_levels_pass() {
    assert_cascade_passes("AgentMcpServer", AGENT_MCP_SERVER_IOA);
}

#[test]
fn cascade_agent_skill_all_levels_pass() {
    assert_cascade_passes("AgentSkill", AGENT_SKILL_IOA);
}

#[test]
fn cascade_agent_tool_all_levels_pass() {
    assert_cascade_passes("AgentTool", AGENT_TOOL_IOA);
}

#[test]
fn cascade_agent_tool_config_all_levels_pass() {
    assert_cascade_passes("AgentToolConfig", AGENT_TOOL_CONFIG_IOA);
}

#[test]
fn cascade_agent_version_all_levels_pass() {
    assert_cascade_passes("AgentVersion", AGENT_VERSION_IOA);
}

// --- Session slice (ADR-0044) ----------------------------------------------

#[test]
fn cascade_session_all_levels_pass() {
    assert_cascade_passes("Session", SESSION_IOA);
}

#[test]
fn cascade_session_resource_all_levels_pass() {
    assert_cascade_passes("SessionResource", SESSION_RESOURCE_IOA);
}

#[test]
fn cascade_session_event_all_levels_pass() {
    assert_cascade_passes("SessionEvent", SESSION_EVENT_IOA);
}

#[test]
fn csdl_parses_and_has_all_entity_types() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    for entity_type in [
        "Environment",
        "EnvironmentAllowedHost",
        "EnvironmentPackage",
        "ManagedAgent",
        "AgentMcpServer",
        "AgentSkill",
        "AgentTool",
        "AgentToolConfig",
        "AgentVersion",
        "Session",
        "SessionResource",
        "SessionEvent",
    ] {
        assert!(
            schema.entity_type(entity_type).is_some(),
            "CSDL should define {entity_type} entity type"
        );
    }
}

#[test]
fn csdl_defines_all_session_lifecycle_actions() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    for action in [
        "StartSession",
        "IdleSession",
        "ResumeSession",
        "RescheduleSession",
        "TerminateSession",
        "ArchiveSession",
    ] {
        assert!(
            schema.action(action).is_some(),
            "CSDL should define {action} bound action"
        );
    }
}

#[test]
fn csdl_session_has_navigation_properties_to_children() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let session = schema
        .entity_type("Session")
        .expect("Session entity type should exist");
    let nav_names: Vec<&str> = session
        .navigation_properties
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    for nav in ["ManagedAgent", "Environment", "Resources", "Events"] {
        assert!(
            nav_names.contains(&nav),
            "Session should have a `{nav}` navigation property, got {nav_names:?}"
        );
    }
}

#[test]
fn csdl_session_numeric_fields_have_correct_types() {
    // Phase 2 intentionally ships without a `numeric_gte` field-invariant
    // predicate. As a fallback sanity check (ADR-0044 Platform Extensions
    // section), confirm that every numeric field on Session uses a numeric
    // CSDL type so that raw JSON serialization cannot smuggle a negative
    // string through the write path.
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let session = schema
        .entity_type("Session")
        .expect("Session entity type should exist");

    let numeric_fields: &[(&str, &[&str])] = &[
        ("ActiveSeconds", &["Edm.Double"]),
        ("DurationSeconds", &["Edm.Double"]),
        ("InputTokens", &["Edm.Int64"]),
        ("OutputTokens", &["Edm.Int64"]),
        ("CacheReadInputTokens", &["Edm.Int64"]),
        ("CacheCreation1hInputTokens", &["Edm.Int64"]),
        ("CacheCreation5mInputTokens", &["Edm.Int64"]),
        ("AgentVersion", &["Edm.Int32"]),
    ];

    for (field, allowed) in numeric_fields {
        let prop = session
            .properties
            .iter()
            .find(|p| p.name == *field)
            .unwrap_or_else(|| panic!("Session.{field} should exist as a CSDL property"));
        assert!(
            allowed.iter().any(|t| prop.type_name == *t),
            "Session.{field} should have numeric type (one of {allowed:?}), got `{}`",
            prop.type_name
        );
    }

    let event = schema
        .entity_type("SessionEvent")
        .expect("SessionEvent entity type should exist");
    let sequence = event
        .properties
        .iter()
        .find(|p| p.name == "Sequence")
        .expect("SessionEvent.Sequence should exist");
    assert_eq!(
        sequence.type_name, "Edm.Int64",
        "SessionEvent.Sequence should be Edm.Int64"
    );
}

#[test]
fn csdl_session_event_has_full_adr_0045_column_set() {
    // ADR-0045 expands SessionEvent from 14 columns / 9 Kind values to 27
    // columns / 20 Kind values. This test pins the new columns so a typo or
    // accidental rename anywhere in the CSDL will fail the cascade.
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let event = schema
        .entity_type("SessionEvent")
        .expect("SessionEvent entity type should exist");

    let required_columns: &[(&str, &str)] = &[
        // Phase 2 baseline
        ("Id", "Edm.String"),
        ("SessionId", "Edm.String"),
        ("Sequence", "Edm.Int64"),
        ("Kind", "Edm.String"),
        ("ProcessedAt", "Edm.DateTimeOffset"),
        ("Content", "Edm.String"),
        ("ToolUseId", "Edm.String"),
        ("ToolName", "Edm.String"),
        ("McpServerName", "Edm.String"),
        ("IsError", "Edm.Boolean"),
        ("EvaluatedPermission", "Edm.String"),
        ("ConfirmationResult", "Edm.String"),
        ("DenyMessage", "Edm.String"),
        ("CreatedAt", "Edm.DateTimeOffset"),
        // ADR-0045 additions (13 new)
        ("CustomToolUseId", "Edm.String"),
        ("McpToolUseId", "Edm.String"),
        ("StopReason", "Edm.String"),
        ("StopReasonEventIds", "Edm.String"),
        ("ErrorKind", "Edm.String"),
        ("ErrorMessage", "Edm.String"),
        ("RetryStatus", "Edm.String"),
        ("ModelRequestStartId", "Edm.String"),
        ("ModelInputTokens", "Edm.Int64"),
        ("ModelOutputTokens", "Edm.Int64"),
        ("ModelCacheCreationInputTokens", "Edm.Int64"),
        ("ModelCacheReadInputTokens", "Edm.Int64"),
        ("ModelSpeed", "Edm.String"),
    ];

    for (name, expected_type) in required_columns {
        let prop = event
            .properties
            .iter()
            .find(|p| p.name == *name)
            .unwrap_or_else(|| {
                panic!("SessionEvent.{name} should exist as a CSDL property (ADR-0045)")
            });
        assert_eq!(
            prop.type_name, *expected_type,
            "SessionEvent.{name} should be {expected_type}, got `{}`",
            prop.type_name
        );
    }

    assert_eq!(
        event.properties.len(),
        required_columns.len(),
        "SessionEvent should have exactly {} columns after ADR-0045; found {} ({:?})",
        required_columns.len(),
        event.properties.len(),
        event
            .properties
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn session_event_kind_enum_has_twenty_members() {
    // ADR-0045: the KindMustBeKnown invariant enumerates every Anthropic
    // event type. Pin the set so an accidental drop or rename fails here.
    let automaton = parse_automaton(SESSION_EVENT_IOA)
        .expect("SessionEvent IOA should parse");

    let kind_invariant = automaton
        .field_invariants
        .iter()
        .find(|i| i.name == "KindMustBeKnown")
        .expect("SessionEvent should declare KindMustBeKnown");

    // Serialize the predicate and extract string literals that the `require`
    // side compares against. We do this via the rendered serde form rather
    // than pattern-matching the enum to stay resilient to grammar additions.
    let json = serde_json::to_value(&kind_invariant.require)
        .expect("FieldPredicate should serialize");

    fn collect_equals<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(literal)) = map.get("equals") {
                    out.push(literal.as_str());
                }
                for (_, v) in map {
                    collect_equals(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_equals(item, out);
                }
            }
            _ => {}
        }
    }

    let mut kinds: Vec<&str> = Vec::new();
    collect_equals(&json, &mut kinds);
    kinds.sort();

    let mut expected: Vec<&str> = vec![
        "user.message",
        "user.interrupt",
        "user.tool_confirmation",
        "user.custom_tool_result",
        "agent.message",
        "agent.thinking",
        "agent.custom_tool_use",
        "agent.tool_use",
        "agent.tool_result",
        "agent.mcp_tool_use",
        "agent.mcp_tool_result",
        "agent.thread_context_compacted",
        "session.status_running",
        "session.status_idle",
        "session.status_rescheduled",
        "session.status_terminated",
        "session.deleted",
        "session.error",
        "span.model_request_start",
        "span.model_request_end",
    ];
    expected.sort();

    assert_eq!(
        kinds, expected,
        "SessionEvent.KindMustBeKnown should enumerate exactly the 20 ADR-0045 kinds"
    );
}

#[test]
fn csdl_defines_archive_managed_agent_action() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    assert!(
        schema.action("ArchiveManagedAgent").is_some(),
        "CSDL should define ArchiveManagedAgent bound action"
    );
}

#[test]
fn csdl_managed_agent_has_navigation_properties_to_children() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let managed_agent = schema
        .entity_type("ManagedAgent")
        .expect("ManagedAgent entity type should exist");
    let nav_names: Vec<&str> = managed_agent
        .navigation_properties
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    for nav in ["McpServers", "Skills", "Tools", "Versions"] {
        assert!(
            nav_names.contains(&nav),
            "ManagedAgent should have a `{nav}` navigation property, got {nav_names:?}"
        );
    }
}

#[test]
fn csdl_defines_archive_environment_action() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    assert!(
        schema.action("ArchiveEnvironment").is_some(),
        "CSDL should define ArchiveEnvironment bound action"
    );
}

#[test]
fn csdl_environment_has_navigation_properties_to_children() {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let env = schema
        .entity_type("Environment")
        .expect("Environment entity type should exist");
    let nav_names: Vec<&str> = env
        .navigation_properties
        .iter()
        .map(|n| n.name.as_str())
        .collect();
    assert!(
        nav_names.contains(&"AllowedHosts"),
        "Environment should have an `AllowedHosts` navigation property, got {nav_names:?}"
    );
    assert!(
        nav_names.contains(&"Packages"),
        "Environment should have a `Packages` navigation property, got {nav_names:?}"
    );
}

#[test]
fn field_invariants_reference_valid_csdl_properties() {
    // For every IOA spec that declares field invariants, verify every field
    // referenced resolves to a real CSDL property on the spec's entity type.
    // Without this cross-check, a typo in a field name would silently degrade
    // a hard constraint into a no-op.
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    let cases: [(&str, &str, bool); 12] = [
        ("Environment", ENVIRONMENT_IOA, true),
        ("EnvironmentAllowedHost", ALLOWED_HOST_IOA, false),
        ("EnvironmentPackage", PACKAGE_IOA, false),
        ("ManagedAgent", MANAGED_AGENT_IOA, true),
        ("AgentMcpServer", AGENT_MCP_SERVER_IOA, true),
        ("AgentSkill", AGENT_SKILL_IOA, true),
        ("AgentTool", AGENT_TOOL_IOA, true),
        ("AgentToolConfig", AGENT_TOOL_CONFIG_IOA, true),
        ("AgentVersion", AGENT_VERSION_IOA, false),
        ("Session", SESSION_IOA, true),
        ("SessionResource", SESSION_RESOURCE_IOA, true),
        ("SessionEvent", SESSION_EVENT_IOA, true),
    ];

    for (entity_name, ioa, expect_invariants) in cases {
        let automaton = parse_automaton(ioa)
            .unwrap_or_else(|e| panic!("{entity_name} IOA should parse: {e}"));
        let entity = schema
            .entity_type(entity_name)
            .unwrap_or_else(|| panic!("{entity_name} entity type should exist"));
        let property_names: Vec<&str> =
            entity.properties.iter().map(|p| p.name.as_str()).collect();

        if expect_invariants {
            assert!(
                !automaton.field_invariants.is_empty(),
                "{entity_name} should declare at least one field invariant"
            );
        }

        for invariant in &automaton.field_invariants {
            for field in invariant.referenced_fields() {
                assert!(
                    property_names.contains(&field.as_str()),
                    "{entity_name} field invariant `{}` references unknown CSDL property: `{field}` (known: {property_names:?})",
                    invariant.name,
                );
            }
        }
    }
}

#[test]
fn cross_invariants_reference_valid_csdl_properties() {
    // Parse cross-invariants.toml and verify every field name referenced on
    // the related-entity side resolves to a real CSDL property on the target
    // entity. Uses the extended ADR-0041 grammar:
    // `related(Environment, EnvironmentId).ConfigType not in ["Local"]`.
    use temper_spec::cross_invariant::parse_related_field_assert;

    let cross =
        parse_cross_invariants(CROSS_INVARIANTS_TOML).expect("cross-invariants should parse");

    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|s| s.namespace == "Temper.Crucible")
        .expect("Temper.Crucible schema should exist");

    assert!(
        !cross.invariants.is_empty(),
        "cross-invariants.toml should declare at least one invariant"
    );

    for inv in &cross.invariants {
        let parsed = parse_related_field_assert(&inv.assertion).unwrap_or_else(|| {
            panic!(
                "cross-invariant `{}` has unparseable assertion `{}`",
                inv.name, inv.assertion
            )
        });

        let target = schema
            .entity_type(&parsed.target_entity)
            .unwrap_or_else(|| {
                panic!(
                    "cross-invariant `{}` references unknown target entity `{}`",
                    inv.name, parsed.target_entity
                )
            });

        let target_properties: Vec<&str> =
            target.properties.iter().map(|p| p.name.as_str()).collect();
        assert!(
            target_properties.contains(&parsed.field_name.as_str()),
            "cross-invariant `{}` references unknown CSDL property `{}` on `{}` (known: {target_properties:?})",
            inv.name,
            parsed.field_name,
            parsed.target_entity,
        );
    }
}
