use super::AppBundle;
use crate::state::PlatformState;

pub(crate) fn os_app_policy_row_id(app_name: &str, relative_path: &str) -> String {
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
    }
    Ok(())
}
