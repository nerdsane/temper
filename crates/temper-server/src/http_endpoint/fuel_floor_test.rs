//! The git upload-pack fuel floor (nerdsane/temper#417).

use super::{GIT_PACK_FUEL_FLOOR, route_from_entity_fields};

#[tokio::test]
async fn git_pack_fuel_floor_can_be_raised_but_not_lowered() {
    // A stored value above the floor is preserved (floor only raises).
    let high = serde_json::json!({
        "PathPrefix": "/{owner}/{repo}.git/git-upload-pack",
        "Methods": "POST",
        "IntegrationModule": "git_upload_pack",
        "MaxFuel": 50_000_000_000_000u64,
    });
    let r = route_from_entity_fields("he-high", &high).unwrap();
    assert_eq!(r.max_fuel, Some(50_000_000_000_000));

    // A non-git module's explicit fuel is untouched by the floor.
    let other = serde_json::json!({
        "PathPrefix": "/api/thing",
        "Methods": "POST",
        "IntegrationModule": "some_other_module",
        "MaxFuel": 5_000_000u64,
    });
    let r = route_from_entity_fields("he-other", &other).unwrap();
    assert_eq!(r.max_fuel, Some(5_000_000));

    // receive-pack (client-controlled ingest input) is NOT floored: a
    // stored value below the floor is kept, and an unset value defaults to
    // 20B; its tight bound caps unauthenticated attacker-controlled work.
    let recv = serde_json::json!({
        "PathPrefix": "/{owner}/{repo}.git/git-receive-pack",
        "Methods": "POST",
        "IntegrationModule": "git_receive_pack",
        "MaxFuel": 20_000_000_000u64,
    });
    let r = route_from_entity_fields("he-recv2", &recv).unwrap();
    assert_eq!(r.max_fuel, Some(20_000_000_000));
}

#[tokio::test]
async fn upload_pack_below_the_floor_is_raised_to_it() {
    let low = serde_json::json!({
        "PathPrefix": "/{owner}/{repo}.git/git-upload-pack",
        "Methods": "POST",
        "IntegrationModule": "git_upload_pack",
        "MaxFuel": 20_000_000_000u64,
    });
    let r = route_from_entity_fields("he-low", &low).unwrap();
    assert_eq!(r.max_fuel, Some(GIT_PACK_FUEL_FLOOR));
}
