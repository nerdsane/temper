use super::*;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");
const PROCESS_IOA: &str = include_str!("../../../../../test-fixtures/specs/process.ioa.toml");
const EFFECT_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Idle", "Ready"]
initial = "Idle"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[state]]
name = "entries"
type = "list"
initial = "[]"

[[action]]
name = "Run"
kind = "input"
from = ["Idle"]
to = "Ready"
effect = [
  { type = "increment", var = "count", amount = "delta" },
  { type = "decrement", var = "count", amount = "delta" },
  { type = "set_counter_from_param", var = "count", param = "total" },
  { type = "list_append", var = "entries" },
  { type = "list_remove_at", var = "entries" },
  { type = "emit", event = "Changed" },
  { type = "trigger", name = "Notify" },
  { type = "schedule", action = "Wake", delay_seconds = 5 },
  { type = "schedule_at", action = "Expire", field = "expires_at" },
  { type = "spawn", entity_type = "Child", entity_id_source = "child_id", initial_action = "Start" },
]

[[action.triggers]]
name = "changed_target"
kind = "entity"
target_entity = "Child"
target_action = "Start"

[action.triggers.resolve_target]
type = "same_id"

[[action]]
name = "Wake"
kind = "input"
from = ["Ready"]
to = "Ready"

[[action]]
name = "Expire"
kind = "input"
from = ["Ready"]
to = "Ready"
"#;
const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["Idle", "Active"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Active"
"#;

fn registry_with(tenant: &str, entity_type: &str, ioa_source: &str) -> SpecRegistry {
    registry_with_specs(tenant, &[(entity_type, ioa_source)])
}

fn registry_with_specs(tenant: &str, specs: &[(&str, &str)]) -> SpecRegistry {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(tenant, csdl, CSDL_XML.to_string(), specs);
    registry
}

fn server_reaction() -> ServerReactionRule {
    ServerReactionRule {
        name: "order-routes-child".into(),
        when: temper_server::trigger::ReactionTrigger {
            entity_type: "Order".into(),
            action: Some("Run".into()),
            to_state: Some("Ready".into()),
            guard: None,
        },
        then: temper_server::trigger::ReactionTarget {
            entity_type: "Child".into(),
            action: "Start".into(),
            params: serde_json::json!({}),
            params_from: Default::default(),
        },
        resolve_target: TargetResolver::SameId,
        principal: None,
    }
}

#[test]
fn parses_actor_backed_type_tokens() {
    let parsed = parse_actor_backed_types(&["Order,Invoice".into(), "Customer".into()])
        .expect("actor-backed type list should parse");
    assert!(!parsed.all);
    assert!(parsed.global_types.contains("Order"));
    assert!(parsed.global_types.contains("Invoice"));
    assert!(parsed.global_types.contains("Customer"));
}

#[test]
fn parses_tenant_scoped_actor_backed_type_tokens() {
    let parsed = parse_actor_backed_types(&["alpha:Order,beta:Invoice".into()])
        .expect("tenant-scoped actor-backed type list should parse");

    assert!(!parsed.all);
    assert!(
        parsed
            .tenant_types
            .contains(&("alpha".into(), "Order".into()))
    );
    assert!(
        parsed
            .tenant_types
            .contains(&("beta".into(), "Invoice".into()))
    );
}

#[test]
fn parses_all_actor_backed_types() {
    assert!(
        parse_actor_backed_types(&["all".into()])
            .expect("all selector should parse")
            .all
    );
    assert!(
        parse_actor_backed_types(&["*".into()])
            .expect("wildcard selector should parse")
            .all
    );
}

#[test]
fn rejects_all_mixed_with_specific_types() {
    let err = parse_actor_backed_types(&["all".into(), "Order".into()]).unwrap_err();
    assert!(err.to_string().contains("cannot be combined"));
}

#[test]
fn collects_compatible_registry_specs() {
    let registry = registry_with("alpha", "Order", ORDER_IOA);
    let definitions = collect_actor_runtime_definitions(&registry, &[])
        .expect("compatible registry specs should collect");

    assert_eq!(definitions.definitions.len(), 1);
    assert_eq!(definitions.definitions[0].entity_type, "Order");
    assert!(definitions.actor_backed_keys.contains("Order"));
}

#[test]
fn collects_tenant_scoped_registry_specs() {
    let registry = registry_with("alpha", "Order", ORDER_IOA);
    let definitions = collect_actor_runtime_definitions(&registry, &["alpha:Order".into()])
        .expect("tenant-scoped registry specs should collect");

    assert_eq!(definitions.definitions.len(), 1);
    assert_eq!(definitions.definitions[0].entity_type, "Order");
    assert!(definitions.actor_backed_keys.contains("alpha:Order"));
}

#[test]
fn rejects_missing_explicit_actor_backed_type() {
    let registry = registry_with("alpha", "Order", ORDER_IOA);
    let err = collect_actor_runtime_definitions(&registry, &["Invoice".into()]).unwrap_err();

    assert!(err.to_string().contains("is not loaded"));
}

#[test]
fn accepts_complete_actor_effect_vocabulary_and_inline_reactions() {
    let registry = registry_with_specs("alpha", &[("Order", EFFECT_IOA), ("Child", CHILD_IOA)]);
    let definitions = collect_actor_runtime_definitions(&registry, &["Order,Child".into()])
        .expect("canonical effects and simple inline reactions must be actor-compatible");

    assert_eq!(definitions.definitions.len(), 2);
    let order = definitions
        .definitions
        .iter()
        .find(|definition| definition.entity_type == "Order")
        .expect("Order actor definition");
    assert_eq!(order.reaction_rules.len(), 1);
    assert_eq!(order.reaction_rules[0].then.entity_type, "Child");
}

#[test]
fn rejects_reaction_target_that_is_not_loaded_and_selected() {
    let registry = registry_with("alpha", "Order", EFFECT_IOA);
    let error = collect_actor_runtime_definitions(&registry, &["Order".into()]).unwrap_err();

    assert!(error.to_string().contains("must be loaded and selected"));
}

#[test]
fn converts_field_and_static_target_resolvers_without_loss() {
    let tenant = TenantId::from("alpha");
    let mut field = server_reaction();
    field.resolve_target = TargetResolver::Field {
        field: "PaymentId".into(),
    };
    let mut static_target = server_reaction();
    static_target.resolve_target = TargetResolver::Static {
        entity_id: "payment-1".into(),
    };

    assert!(matches!(
        actor_reaction_rule(&tenant, &field)
            .expect("field resolver conversion")
            .resolve_target,
        ActorTargetResolver::Field { field } if field == "PaymentId"
    ));
    assert!(matches!(
        actor_reaction_rule(&tenant, &static_target)
            .expect("static resolver conversion")
            .resolve_target,
        ActorTargetResolver::Static { entity_id } if entity_id == "payment-1"
    ));
}

#[test]
fn rejects_every_unpreserved_rich_reaction_semantic() {
    let tenant = TenantId::from("alpha");
    let mut guard = server_reaction();
    guard.when.guard = Some(temper_server::trigger::ReactionGuard::StateIn {
        values: vec!["Ready".into()],
    });
    let mut principal = server_reaction();
    principal.principal = Some("order-agent".into());
    let mut params = server_reaction();
    params.then.params = serde_json::json!({"source": "runtime"});
    let mut params_from = server_reaction();
    params_from
        .then
        .params_from
        .insert("payment_id".into(), "PaymentId".into());
    let mut create = server_reaction();
    create.resolve_target = TargetResolver::Create;

    for (semantic, rule) in [
        ("guard", guard),
        ("principal", principal),
        ("params", params),
        ("params_from", params_from),
        ("create", create),
    ] {
        assert!(
            actor_reaction_rule(&tenant, &rule).is_err(),
            "{semantic} must fail startup instead of being silently discarded"
        );
    }
}

#[test]
fn rejects_legacy_integrations_without_blaming_supported_effects() {
    let registry = registry_with("alpha", "Process", PROCESS_IOA);
    let err = collect_actor_runtime_definitions(&registry, &["Process".into()]).unwrap_err();

    assert!(err.to_string().contains("legacy integrations"));
}

#[test]
fn rejects_conflicting_same_named_specs_across_tenants() {
    let mut registry = registry_with("alpha", "Order", ORDER_IOA);
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let beta_ioa = ORDER_IOA.replace("initial = \"Draft\"", "initial = \"Submitted\"");
    registry.register_tenant("beta", csdl, CSDL_XML.to_string(), &[("Order", &beta_ioa)]);

    let err = collect_actor_runtime_definitions(&registry, &["Order".into()]).unwrap_err();

    assert!(
        err.to_string()
            .contains("different IOA or reaction definitions")
    );
}
