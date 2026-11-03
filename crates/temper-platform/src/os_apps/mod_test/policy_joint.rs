//! Tenant-wide Cedar changes must preserve other apps' joint properties.
use super::*;

const APP: &str = r#"
[automaton]
name = "App"
states = ["New", "Live"]
initial = "New"
[[action]]
name = "Start"
kind = "input"
from = ["New"]
to = "Live"
[[action.triggers]]
name = "bring"
kind = "entity"
target_entity = "Place"
target_action = "Bring"
principal = "place"
[action.triggers.resolve_target]
kind = "same_id"
"#;
const PLACE: &str = r#"
[automaton]
name = "Place"
states = ["New", "Live"]
initial = "New"
[[action]]
name = "Bring"
kind = "input"
from = ["New"]
to = "Live"
[[liveness]]
name = "brought"
eventually = "App[Id].status == 'Live' => status == 'Live'"
"#;
const ALLOW: &str = r#"permit(principal, action, resource);"#;
const DENY: &str = r#"
permit(principal, action, resource);
forbid(principal, action == Action::"Bring", resource);
"#;

fn fixture() -> PlatformState {
    fixture_with_place(PLACE)
}

fn fixture_with_place(place: &str) -> PlatformState {
    let state = PlatformState::new(None);
    let csdl = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
    state.registry.write().unwrap().register_tenant(
        "policy-owner",
        parse_csdl(csdl).expect("the CSDL parses"),
        csdl.to_string(),
        &[("App", APP), ("Place", place)],
    );
    state
}

#[tokio::test]
async fn policy_only_install_checks_existing_apps_in_its_tenant() {
    let state = fixture();
    let unproved = install_checks::prove_to_the_end(
        &state,
        "policy-owner",
        "policy-only",
        &[],
        false,
        Some(ALLOW),
    )
    .await
    .expect("the existing reaction can bring its record");
    assert!(unproved.is_empty(), "{unproved:?}");
    let error = install_checks::prove_to_the_end(
        &state,
        "policy-owner",
        "policy-only",
        &[],
        false,
        Some(DENY),
    )
    .await
    .expect_err("a forbid must not strand another app's record");
    assert!(error.contains("brought"), "{error}");
    let unproved = install_checks::prove_to_the_end(
        &state,
        "other-tenant",
        "policy-only",
        &[],
        false,
        Some(DENY),
    )
    .await
    .expect("other tenants' specs do not constrain this install");
    assert!(unproved.is_empty(), "{unproved:?}");
}

#[tokio::test]
async fn policy_checks_use_incoming_specs_over_active_versions() {
    let state = fixture();
    let replacement = PLACE.replace("App[Id].status == 'Live' => status == 'Live'", "true");
    let unproved = install_checks::prove_to_the_end(
        &state,
        "policy-owner",
        "updated",
        &[("Place".into(), replacement)],
        true,
        Some(DENY),
    )
    .await
    .expect("the incoming spec replaces the old property before checking");
    // The incoming property is `eventually true`, which the forbid cannot
    // break, while the active one is refused under the same Cedar (see
    // `policy_only_install_checks_existing_apps_in_its_tenant`). App's
    // reaction into Place is outside the new property's scope, so the walk
    // sends it as an outside move, and the property holds with it.
    assert!(unproved.is_empty(), "{unproved:?}");
}

#[tokio::test]
async fn a_new_permit_checks_existing_apps_invariants() {
    let place = format!(
        "{PLACE}\n[[invariant]]\nname = \"only_after_start\"\nassert = \"status == 'Live' => App[Id].status == 'Live'\"\n"
    );
    let state = fixture_with_place(&place);
    let service_only = r#"
permit(principal, action == Action::"Start", resource);
permit(principal, action == Action::"Bring", resource is Place) when {
    principal == Agent::"service:place" && context has triggerSourceEntityId &&
    resource has id && context.triggerSourceEntityId == resource.id
};
"#;
    let unproved = install_checks::prove_to_the_end(
        &state,
        "policy-owner",
        "policy-only",
        &[],
        false,
        Some(service_only),
    )
    .await
    .expect("only the reaction can finish after Start");
    assert!(unproved.is_empty(), "{unproved:?}");
    let error = install_checks::prove_to_the_end(
        &state,
        "policy-owner",
        "policy-only",
        &[],
        false,
        Some(ALLOW),
    )
    .await
    .expect_err("a permit must not let an outside caller break another app's invariant");
    assert!(error.contains("only_after_start"), "{error}");
}
