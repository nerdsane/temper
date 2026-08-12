use super::{AppBundle, bundle_policy_label};
use crate::state::PlatformState;

/// Durable `policies` row id for one Cedar file in an app bundle.
///
/// This is the same string the Cedar engine labels the file's statements with,
/// so a denial recovered from durable storage after a restart still reads
/// `katagami-commons/art_style.cedar#2` (ARN-286). Keeping the two in sync is
/// the point — a row id that differs from the load label would give the same
/// policy two different names depending on which path loaded it.
pub(crate) fn os_app_policy_row_id(app_name: &str, relative_path: &str) -> String {
    bundle_policy_label(app_name, relative_path)
}

/// The pre-ARN-286 row id: a dash-slugged `{app}-{path}` with the `.cedar`
/// suffix dropped. Rows written under it are deleted once the same policy has
/// been re-saved under [`os_app_policy_row_id`], so a tenant does not end up
/// loading both copies.
fn legacy_os_app_policy_row_id(app_name: &str, relative_path: &str) -> String {
    let source = relative_path
        .trim_start_matches('/')
        .strip_prefix("policies/")
        .unwrap_or_else(|| relative_path.trim_start_matches('/'))
        .strip_suffix(".cedar")
        .unwrap_or(relative_path);
    let mut slug = String::new();
    for ch in source.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            slug.push(ch);
        } else if ch == '/' || ch == '.' {
            slug.push('-');
        } else {
            slug.push('_');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("{app_name}-policy")
    } else {
        format!("{app_name}-{slug}")
    }
}

pub(super) async fn persist_bundle_policy_rows(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<(), String> {
    if bundle.cedar_policy_sources.is_empty() {
        return Ok(());
    }
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(());
    };
    let created_by = format!("os-app:{app_name}");
    for source in &bundle.cedar_policy_sources {
        let cedar_text = source.text.trim();
        if cedar_text.is_empty() {
            continue;
        }
        let policy_id = os_app_policy_row_id(app_name, &source.relative_path);
        policy_store
            .save_policy(tenant, &policy_id, cedar_text, &created_by)
            .await
            .map_err(|error| {
                format!(
                    "Failed to persist OS app Cedar policy row '{policy_id}' for '{app_name}': {error}"
                )
            })?;

        // Drop the row this policy used to live under, now that it has been
        // re-saved under its label. Leaving it would load the same statements
        // twice, once unnamed.
        // Fatal, not warn-only: a surviving legacy row is loaded again on the
        // next recovery, so the OLD generation of these statements comes back
        // alongside the new one. With Cedar's forbid-overrides-permit that can
        // reactivate a forbid the operator believes they deleted — stale
        // authorization restored by a restart, with no signal at install time.
        let legacy_id = legacy_os_app_policy_row_id(app_name, &source.relative_path);
        if legacy_id != policy_id {
            policy_store
                .delete_policy(tenant, &legacy_id)
                .await
                .map_err(|error| {
                    format!(
                        "Failed to remove superseded OS app Cedar policy row '{legacy_id}' for '{app_name}': {error}"
                    )
                })?;
        }
    }
    Ok(())
}
