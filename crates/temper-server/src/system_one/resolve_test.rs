use super::*;
use crate::blobs::{BlobReadSource, OverflowBlobWrite, blob_ref_value};
use crate::entity_actor::EntityActor;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::scheduler::install_deterministic_context;

const SPEC: &str = r#"
[automaton]
name = "Case"
states = ["Open", "Escalated"]
initial = "Open"

[[state]]
name = "Messages"
type = "string"
initial = ""

[[action]]
name = "Escalate"
kind = "input"
from = ["Open"]
to = "Escalated"
guard = [{ type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { human = { type = "noul", instructions = "Has a human been requested?" } }, assert = "answers.human.noul >= 0.8" }]
"#;

fn caller(id: &str) -> AgentContext {
    AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            id,
            PrincipalKind::Customer,
            None,
            None,
            None,
            None,
        )),
        ..Default::default()
    }
}

#[test]
fn distinct_humans_do_not_share_receipt_identity() {
    let alice = caller("alice");
    let bob = caller("bob");
    assert!(alice.agent_id.is_none());
    assert!(bob.agent_id.is_none());
    assert_ne!(
        canonical_principal_identity(&alice).unwrap(),
        canonical_principal_identity(&bob).unwrap()
    );
}

#[test]
fn principal_binding_ignores_metadata_and_orders_authority_attributes() {
    let original = caller("alice");
    let mut altered = original.clone();
    altered.agent_id = Some("forged-agent-id".into());
    altered.agent_type = Some("forged-agent-type".into());
    altered.session_id = Some("another-session".into());
    altered.security_ctx.as_mut().unwrap().correlation_id = "another-correlation".into();
    assert_eq!(
        canonical_principal_identity(&original).unwrap(),
        canonical_principal_identity(&altered).unwrap()
    );
    let left = original.security_ctx.unwrap();
    let mut right = left.clone();
    right.principal.attributes.clear();
    right
        .principal
        .attributes
        .insert("zeta".into(), json!(true));
    right
        .principal
        .attributes
        .insert("alpha".into(), json!(true));
    let mut left = left;
    left.principal.attributes.clear();
    left.principal
        .attributes
        .insert("alpha".into(), json!(true));
    left.principal.attributes.insert("zeta".into(), json!(true));
    assert_eq!(
        canonical_principal_identity(&AgentContext {
            security_ctx: Some(left),
            ..Default::default()
        })
        .unwrap(),
        canonical_principal_identity(&AgentContext {
            security_ctx: Some(right),
            ..Default::default()
        })
        .unwrap()
    );
}

#[test]
fn projection_publishes_current_authoritative_state() {
    let _clock = install_deterministic_context(800);
    let table = TransitionTable::from_ioa_source(SPEC);
    let mut state = EntityActor::build_initial_state("Case", "case-a", &table, &json!({}));
    state.status = "Escalated".into();
    state.counters.insert("rounds".into(), 4);
    state.booleans.insert("assigned".into(), true);
    state.lists.insert("reviewers".into(), vec!["alice".into()]);
    state.fields =
        json!({"Status":"Open", "Id":"forged", "rounds":1, "assigned":false, "reviewers":[]});
    let projected = entity_projection(&state);
    assert_eq!(projected["Status"], "Escalated");
    assert_eq!(projected["Id"], "case-a");
    assert_eq!(projected["rounds"], 4);
    assert_eq!(projected["assigned"], true);
    assert_eq!(projected["reviewers"], json!(["alice"]));
    assert_eq!(
        state.fields["Status"], "Open",
        "projection must not mutate actor state"
    );
}

fn overflow(text: &str) -> OverflowBlobWrite {
    use sha2::{Digest, Sha256};
    let body = serde_json::to_vec(&json!(text)).unwrap();
    let key = format!("field-overflow/sha256/{:x}.json", Sha256::digest(&body));
    OverflowBlobWrite {
        key,
        body,
        ttl_seconds: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn declared_long_field_is_evaluated_as_verified_content() {
    let _clock = install_deterministic_context(801);
    let table = TransitionTable::from_ioa_source(SPEC);
    let guard = collect_guards(&table, "Escalate")[0];
    let text = format!(
        "{} Please connect me with a human.",
        "Conversation. ".repeat(1000)
    );
    let blob = overflow(&text);
    let entity = json!({"Messages": blob_ref_value(&blob.key, blob.body.len())});
    let context = resolve_context(
        guard,
        &entity,
        &json!({}),
        &BlobReadSource::Staged {
            store: None,
            legacy: None,
            blobs: &[blob],
        },
    )
    .await
    .unwrap();
    assert_eq!(context, json!(text));
    assert_eq!(guard.request(context)["state"], json!(text));
}

#[tokio::test(flavor = "current_thread")]
async fn unavailable_or_oversized_context_cannot_be_evaluated_as_descriptor() {
    let _clock = install_deterministic_context(802);
    let table = TransitionTable::from_ioa_source(SPEC);
    let guard = collect_guards(&table, "Escalate")[0];
    let missing = overflow("Please connect me with a human.");
    let entity = json!({"Messages": blob_ref_value(&missing.key, missing.body.len())});
    assert!(
        resolve_context(
            guard,
            &entity,
            &json!({}),
            &BlobReadSource::Staged {
                store: None,
                legacy: None,
                blobs: &[],
            }
        )
        .await
        .is_err()
    );
    let huge = overflow(&"a".repeat(super::super::provider::SYSTEM_ONE_REQUEST_BYTE_BUDGET));
    let entity = json!({"Messages": blob_ref_value(&huge.key, huge.body.len())});
    assert!(
        resolve_context(
            guard,
            &entity,
            &json!({}),
            &BlobReadSource::Staged {
                store: None,
                legacy: None,
                blobs: &[huge],
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parameter_descriptors_are_data_not_storage_capabilities() {
    let _clock = install_deterministic_context(803);
    let table = TransitionTable::from_ioa_source(SPEC);
    let mut guard = collect_guards(&table, "Escalate")[0].clone();
    guard.state = json!({"ref":"params.Input"});
    let hidden = overflow("Tenant-private material from a different entity.");
    let supplied = blob_ref_value(&hidden.key, hidden.body.len());
    let context = resolve_context(
        &guard,
        &json!({}),
        &json!({"Input":supplied}),
        &BlobReadSource::Staged {
            store: None,
            legacy: None,
            blobs: &[hidden],
        },
    )
    .await
    .unwrap();
    assert_eq!(
        context, supplied,
        "caller input became a kernel storage read"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_constraint_preflight_uses_verified_overflow_content() {
    let _clock = install_deterministic_context(804);
    let spec = SPEC.replace(
        "kind = \"input\"",
        "kind = \"input\"\nparams = [\"expected\"]\nconstraints = [{ kind = \"param_equals_field\", param = \"expected\", field = \"Messages\" }]",
    );
    let table = TransitionTable::from_ioa_source(&spec);
    let text = "Conversation. ".repeat(1000);
    let blob = overflow(&text);
    let mut state = EntityActor::build_initial_state("Case", "case-a", &table, &json!({}));
    state.fields = json!({"Messages":blob_ref_value(&blob.key, blob.body.len())});
    let source = BlobReadSource::Staged {
        store: None,
        legacy: None,
        blobs: &[blob],
    };
    assert!(
        validate_action_input(
            &table,
            &state,
            "Escalate",
            &json!({"expected":text}),
            &source
        )
        .await
        .is_ok()
    );
    assert!(
        validate_action_input(
            &table,
            &state,
            "Escalate",
            &json!({"expected":"different"}),
            &source
        )
        .await
        .is_err()
    );
}
