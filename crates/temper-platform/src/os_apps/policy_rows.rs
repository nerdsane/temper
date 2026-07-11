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
    if state.server.policy_store().is_none() {
        return Ok(());
    }
    let created_by = format!("os-app:{app_name}");
    let mut entries = Vec::new();
    for source in &bundle.cedar_policy_sources {
        let cedar_text = source.text.trim();
        if cedar_text.is_empty() {
            continue;
        }
        let policy_id = os_app_policy_row_id(app_name, &source.relative_path);
        entries.push((policy_id, cedar_text));
    }
    let upserts = entries
        .iter()
        .map(
            |(policy_id, cedar_text)| temper_server::authz::PolicyEntryUpsert {
                policy_id,
                cedar_text,
                created_by: &created_by,
            },
        )
        .collect::<Vec<_>>();
    temper_server::authz::upsert_policy_entries(&state.server, tenant, &upserts)
        .await
        .map(|_| ())
        .map_err(|error| {
            format!("Failed to publish OS app Cedar policies for '{app_name}': {error}")
        })
}

pub(super) async fn activate_installed_policy_rows(
    state: &PlatformState,
    tenant: &str,
    policy_text: &str,
) {
    if state.server.policy_store().is_some() {
        if let Err(e) =
            temper_server::authz::load_and_activate_tenant_policies(&state.server, tenant).await
        {
            tracing::warn!(
                tenant,
                error = %e,
                "Failed to activate durable tenant Cedar policies after os-app install"
            );
        }
        return;
    }

    if let Err(e) = state
        .server
        .authz
        .reload_tenant_policies(tenant, policy_text)
    {
        tracing::warn!(
            tenant,
            error = %e,
            "Failed to reload tenant Cedar policies after os-app install"
        );
        return;
    }
    let mut policies = state
        .server
        .tenant_policies
        .write()
        .expect("tenant policy cache lock poisoned");
    policies.insert(tenant.to_string(), policy_text.to_string());
}
