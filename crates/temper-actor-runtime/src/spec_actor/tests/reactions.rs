use super::*;
use prost::Message as _;

const ROUTED_EFFECT_SPEC: &str = r#"
[automaton]
name = "Router"
states = ["Idle", "Ready"]
initial = "Idle"

[[action]]
name = "Route"
kind = "input"
from = ["Idle"]
to = "Ready"
effect = [{ type = "emit", event = "Changed" }]
"#;

const ROUTED_AND_SCHEDULED_EFFECT_SPEC: &str = r#"
[automaton]
name = "Router"
states = ["Idle", "Ready"]
initial = "Idle"

[[action]]
name = "Route"
kind = "input"
from = ["Idle"]
to = "Ready"
effect = [
  { type = "emit", event = "Changed" },
  { type = "schedule", action = "Wake", delay_seconds = 1 },
]

[[action]]
name = "Wake"
kind = "input"
from = ["Ready"]
to = "Ready"
"#;

fn reaction(
    name: &str,
    action: Option<&str>,
    to_state: Option<&str>,
    target: &str,
    resolver: TargetResolver,
) -> ReactionRule {
    ReactionRule {
        name: name.into(),
        when: temper_runtime::reaction::ReactionTrigger {
            entity_type: "Router".into(),
            action: action.map(str::to_string),
            to_state: to_state.map(str::to_string),
        },
        then: temper_runtime::reaction::ReactionTarget {
            entity_type: target.into(),
            action: "Receive".into(),
        },
        resolve_target: resolver,
    }
}

#[tokio::test]
async fn reactions_preserve_fanout_wildcards_state_filters_and_targets() {
    let rules = vec![
        reaction(
            "same",
            Some("Changed"),
            Some("Ready"),
            "SameTarget",
            TargetResolver::SameId,
        ),
        reaction(
            "field",
            Some("Changed"),
            None,
            "FieldTarget",
            TargetResolver::Field {
                field: "target_namespace".into(),
            },
        ),
        reaction(
            "static",
            Some("Changed"),
            None,
            "StaticTarget",
            TargetResolver::Static {
                entity_id: "static-target".into(),
            },
        ),
        reaction(
            "create",
            Some("Changed"),
            None,
            "CreatedTarget",
            TargetResolver::CreateIfMissing {
                id_field: "child_id".into(),
            },
        ),
        reaction(
            "wildcard",
            None,
            None,
            "WildcardTarget",
            TargetResolver::SameId,
        ),
        reaction(
            "wrong-state",
            Some("Changed"),
            Some("Idle"),
            "WrongStateTarget",
            TargetResolver::SameId,
        ),
    ];
    let actor = SpecDrivenActor::from_ioa(ROUTED_EFFECT_SPEC, ReactionRegistry::from(rules))
        .expect("routed effect spec must parse");
    assert_eq!(
        actor.reactions().lookup("Router", "Changed", "Ready").len(),
        5
    );
    let handle = ActorHandle::new("default/parent-1", "Router");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(
                1,
                &handle,
                "Route",
                serde_json::json!({
                    "target_namespace": "field-target",
                    "child_id": "child-1",
                }),
            ),
        )
        .await
        .expect("reaction fanout must be buffered");

    let tells = context.take_pending_tells().await;
    let targets: std::collections::BTreeSet<_> = tells
        .iter()
        .map(|tell| (tell.to.namespace.as_str(), tell.to.actor_type.as_str()))
        .collect();
    assert_eq!(targets.len(), 5, "unexpected reaction targets: {targets:?}");
    assert_eq!(tells.len(), 6);
    assert_eq!(
        tells
            .iter()
            .filter(|tell| tell.to.actor_type == "WildcardTarget")
            .count(),
        2,
        "the wildcard must receive both declared and translated emitted events"
    );
    assert!(targets.contains(&("default/parent-1", "SameTarget")));
    assert!(targets.contains(&("default/field-target", "FieldTarget")));
    assert!(targets.contains(&("default/static-target", "StaticTarget")));
    assert!(targets.contains(&("default/child-1", "CreatedTarget")));
    assert!(targets.contains(&("default/parent-1", "WildcardTarget")));
    assert!(
        !targets
            .iter()
            .any(|(_, actor_type)| *actor_type == "WrongStateTarget")
    );

    let spawns = context.take_pending_spawns().await;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].handle.namespace, "default/child-1");
    assert_eq!(spawns[0].handle.actor_type, "CreatedTarget");
}

#[tokio::test]
async fn empty_parameter_reaction_does_not_copy_source_fields() {
    let actor = SpecDrivenActor::from_ioa(
        ROUTED_EFFECT_SPEC,
        ReactionRegistry::from(vec![reaction(
            "same",
            Some("Changed"),
            None,
            "Target",
            TargetResolver::SameId,
        )]),
    )
    .expect("routed effect spec must parse");
    let handle = ActorHandle::new("default/source-1", "Router");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(
                1,
                &handle,
                "Route",
                serde_json::json!({"shared": "source-value"}),
            ),
        )
        .await
        .expect("empty-parameter reaction must route");

    let tells = context.take_pending_tells().await;
    assert_eq!(tells.len(), 1);
    let routed = SpecMessage::decode(tells[0].payload.as_slice()).expect("routed message");
    assert!(
        routed.params.is_empty(),
        "source fields must not become target params"
    );
}

#[tokio::test]
async fn unresolved_field_target_preserves_successful_source_transition() {
    let actor = SpecDrivenActor::from_ioa(
        ROUTED_EFFECT_SPEC,
        ReactionRegistry::from(vec![reaction(
            "missing-field",
            Some("Changed"),
            None,
            "Target",
            TargetResolver::Field {
                field: "missing_target".into(),
            },
        )]),
    )
    .expect("routed effect spec must parse");
    let handle = ActorHandle::new("default/source-1", "Router");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(1, &handle, "Route", serde_json::json!({})),
        )
        .await
        .expect("unresolved reaction must not roll back the source action");

    let persisted: SpecActorState = serde_json::from_slice(&state).expect("source state");
    assert_eq!(persisted.status, "Ready");
    assert!(context.take_pending_tells().await.is_empty());
}

#[tokio::test]
async fn create_if_missing_derives_target_from_source_when_field_is_absent() {
    let actor = SpecDrivenActor::from_ioa(
        ROUTED_EFFECT_SPEC,
        ReactionRegistry::from(vec![reaction(
            "derived-target",
            Some("Changed"),
            None,
            "Target",
            TargetResolver::CreateIfMissing {
                id_field: "missing_target".into(),
            },
        )]),
    )
    .expect("routed effect spec must parse");
    let handle = ActorHandle::new("default/source-1", "Router");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(1, &handle, "Route", serde_json::json!({})),
        )
        .await
        .expect("derived create-if-missing reaction must route");

    let spawns = context.take_pending_spawns().await;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].handle.namespace, "default/source-1-derived");
}

#[tokio::test]
async fn admitted_reaction_fanout_plus_schedule_fits_command_budget() {
    let rules: Vec<ReactionRule> = (0..temper_runtime::reaction::MAX_REACTIONS_PER_ACTOR)
        .map(|index| {
            reaction(
                &format!("fanout-{index}"),
                Some("Changed"),
                None,
                "Target",
                TargetResolver::SameId,
            )
        })
        .collect();
    let actor = SpecDrivenActor::from_ioa(
        ROUTED_AND_SCHEDULED_EFFECT_SPEC,
        ReactionRegistry::from(rules),
    )
    .expect("fanout spec must parse");
    let handle = ActorHandle::new("default/source-1", "Router");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(1, &handle, "Route", serde_json::json!({})),
        )
        .await
        .expect("accepted fanout plus schedule must not panic or overflow");

    assert_eq!(
        context.take_pending_tells().await.len(),
        temper_runtime::reaction::MAX_REACTIONS_PER_ACTOR + 1
    );
}
