use crate::registry::{EntityVerificationResult, VerificationStatus};
use crate::state::ServerState;

pub(super) fn restore_unchanged_verification(
    state: &ServerState,
    tenant: &str,
    entity_name: &str,
    cached: &EntityVerificationResult,
) {
    assert!(
        cached.all_passed,
        "unchanged skip is only for specs that already passed"
    );
    assert!(!entity_name.is_empty(), "entity name is required");
    if let Ok(mut registry) = state.registry.write() {
        registry.set_verification_status(
            &tenant.into(),
            entity_name,
            VerificationStatus::Completed(cached.clone()),
        );
    }
}

pub(super) fn cached_verification_line(
    entity_name: &str,
    cached: &EntityVerificationResult,
) -> String {
    assert!(
        cached.all_passed,
        "cached verification line is only for passed specs"
    );
    let levels_json: Vec<serde_json::Value> = cached
        .levels
        .iter()
        .map(|level| {
            let mut obj = serde_json::json!({
                "level": &level.level,
                "passed": level.passed,
                "summary": &level.summary,
            });
            if let Some(details) = &level.details {
                obj["details"] = serde_json::to_value(details).unwrap_or_default();
            }
            obj
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "type": "verification_result",
        "entity": entity_name,
        "all_passed": cached.all_passed,
        "cached": true,
        "reason": "unchanged_verified",
        "levels": levels_json,
    }))
    .unwrap() // ci-ok: infallible serialization
        + "\n"
}
