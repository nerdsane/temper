//! The app's own policy rows: written after the bundle is live, old ones deleted last.

use super::{AppBundle, SupersededAppPolicies, os_app_policy_owner, os_app_policy_row_id};
use crate::state::PlatformState;

pub(in crate::os_apps) async fn persist_bundle_policy_rows(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
    superseded: &SupersededAppPolicies,
) -> Result<(), String> {
    if bundle.cedar_policy_sources.is_empty() {
        return Ok(());
    }
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(());
    };
    let created_by = os_app_policy_owner(app_name);
    for source in &bundle.cedar_policy_sources {
        let cedar_text = source.text.trim();
        if cedar_text.is_empty() {
            continue;
        }
        let policy_id = os_app_policy_row_id(app_name, &source.relative_path);
        // `save_policy` skips an insert when an enabled row already has
        // this cedar_text (ARN-286/399). Reinstall must not grow duplicates.
        policy_store
            .save_policy(tenant, &policy_id, cedar_text, &created_by)
            .await
            .map_err(|error| {
                format!(
                    "Failed to persist OS app Cedar policy row '{policy_id}' for '{app_name}': {error}"
                )
            })?;
    }
    // A whole-set replacement disabled them; the bundle is live again, so they load at boot.
    for id in &superseded.disabled_row_ids {
        let enabled = policy_store.toggle_policy_enabled(tenant, id, true).await;
        enabled.map_err(|error| format!("Failed to enable Cedar policy row '{id}': {error}"))?;
    }
    Ok(())
}

/// Delete the rows of source files the bundle no longer has.
///
/// Runs after the bundle's rows are written, so the app is never left with
/// fewer policies than either version.
///
/// # Errors
///
/// Returns an error when a row cannot be deleted.
pub(in crate::os_apps) async fn delete_dropped_rows(
    state: &PlatformState,
    tenant: &str,
    superseded: &SupersededAppPolicies,
) -> Result<(), String> {
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(());
    };
    for policy_id in &superseded.removed_row_ids {
        policy_store
            .delete_policy(tenant, policy_id)
            .await
            .map_err(|error| format!("Failed to delete Cedar policy row '{policy_id}': {error}"))?;
    }
    Ok(())
}
