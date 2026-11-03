use std::collections::BTreeSet;

use temper_authz::statements::{StatementKey, remove_statements, statement_keys};
use temper_server::storage::PolicyStoreRow;

use super::{AppBundle, merge_bundle_policies};
use crate::state::PlatformState;

mod writes;
pub(super) use writes::{delete_dropped_rows, persist_bundle_policy_rows};

/// Row written by `PUT /api/tenants/{tenant}/policies`. At boot it replaces the
/// legacy aggregate, so it is a snapshot of every app's Cedar, not an owner.
const PRIMARY_POLICY_ID: &str = "primary";

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

/// An app's policy file with each statement labelled `{app}/{file}#{n}`, the
/// file named as under `policies/`, so a denial names the rule that decided it
/// (`statements::label_statements`). Text that does not parse is kept as it is,
/// for the install's own check to refuse.
pub(super) fn labelled_policy_text(app_name: &str, relative_path: &str, text: String) -> String {
    let file = relative_path
        .trim_start_matches('/')
        .strip_prefix("policies/")
        .unwrap_or(relative_path);
    temper_authz::statements::label_statements(&text, &format!("{app_name}/{file}")).unwrap_or(text)
}

fn os_app_policy_owner(app_name: &str) -> String {
    format!("os-app:{app_name}")
}

/// The Cedar a reinstall of one app replaces.
///
/// An earlier install of the app recorded each of its policy files as a row.
/// When the bundle changes, the statements in those rows are superseded: they
/// leave the live set, the legacy aggregate and any `primary` snapshot, and the
/// bundle's statements take their place. Otherwise an old forbid outlives its
/// replacement (forbid beats every permit), or a statement stays live twice,
/// once under an old label. Statements are compared parsed, ignoring layout
/// and labels.
#[derive(Debug, Default)]
pub(super) struct SupersededAppPolicies {
    /// This app's previous statements that no other owner holds; the live
    /// text loses them and gets the bundle's statements in their place.
    statements: BTreeSet<StatementKey>,
    /// This app's rows whose source file is gone from the new bundle.
    removed_row_ids: Vec<String>,
    /// This app's disabled rows that the new bundle still has.
    disabled_row_ids: Vec<String>,
    /// The tenant's `primary` snapshot row, when one exists.
    primary: Option<PolicyStoreRow>,
}

impl SupersededAppPolicies {
    pub(super) fn has_statements(&self) -> bool {
        !self.statements.is_empty()
    }

    /// The tenant's Cedar after installing `bundle`: `live_text` without the
    /// superseded statements, plus the bundle's policies.
    ///
    /// # Errors
    ///
    /// Returns an error when the live text does not parse or the result is
    /// not valid Cedar; the install then stops before persisting anything.
    pub(super) fn replace_in(
        &self,
        state: &PlatformState,
        live_text: &str,
        bundle: &AppBundle,
    ) -> Result<String, String> {
        let kept = remove_statements(live_text, &self.statements)
            .map_err(|error| format!("Failed to read the tenant's live Cedar: {error}"))?;
        let combined = merge_bundle_policies(&kept, &bundle.cedar_policies);
        if self.has_statements() {
            state
                .server
                .authz
                .validate_tenant_policies(&combined)
                .map_err(|error| {
                    format!("Cedar after replacing the app's previous policies is invalid: {error}")
                })?;
        }
        Ok(combined)
    }
}

/// Read which of `app_name`'s previously installed statements `bundle` replaces.
///
/// A statement another enabled row holds (an approval, a hand-added row or
/// another app, but not the `primary` snapshot) is kept. Without a durable
/// policy store nothing is recorded per app, so nothing is superseded.
///
/// # Errors
///
/// Returns an error when the tenant's policy rows cannot be read or parsed.
pub(super) async fn superseded_app_policies(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<SupersededAppPolicies, String> {
    let Some(policy_store) = state.server.policy_store() else {
        return Ok(SupersededAppPolicies::default());
    };
    let rows = policy_store
        .load_policies_for_tenant(tenant)
        .await
        .map_err(|error| format!("Failed to read Cedar policy rows for '{tenant}': {error}"))?;

    let owner = os_app_policy_owner(app_name);
    let mut bundle_statements = BTreeSet::new();
    let mut bundle_row_ids = BTreeSet::new();
    for source in &bundle.cedar_policy_sources {
        if source.text.trim().is_empty() {
            continue;
        }
        bundle_statements.extend(parsed_statements(&source.relative_path, &source.text)?);
        bundle_row_ids.insert(os_app_policy_row_id(app_name, &source.relative_path));
    }
    let mut previous = BTreeSet::new();
    let mut others = Vec::new();
    let mut superseded = SupersededAppPolicies::default();
    for row in &rows {
        if row.created_by == owner {
            previous.extend(parsed_statements(&row.policy_id, &row.cedar_text)?);
            if !bundle_row_ids.contains(&row.policy_id) {
                superseded.removed_row_ids.push(row.policy_id.clone());
            } else if !row.enabled {
                superseded.disabled_row_ids.push(row.policy_id.clone());
            }
        } else if row.policy_id == PRIMARY_POLICY_ID {
            // Boot ignores a disabled snapshot, so only an enabled one matters.
            superseded.primary = row.enabled.then(|| row.clone());
        } else if row.enabled {
            others.push(row);
        }
    }
    // A reinstall that drops nothing and whose text, labels included, is
    // already live (every boot) replaces nothing and reads no other row.
    let live = super::cached_or_active_tenant_policy_text(state, tenant);
    let already_live = bundle
        .cedar_policies
        .iter()
        .all(|policy| live.contains(policy.trim()));
    if previous.is_subset(&bundle_statements) && already_live {
        return Ok(superseded);
    }
    let mut held_by_others = BTreeSet::new();
    for row in others {
        // A row that does not parse holds no live statement: boot cannot load it.
        match parsed_statements(&row.policy_id, &row.cedar_text) {
            Ok(statements) => held_by_others.extend(statements),
            Err(error) => {
                tracing::warn!(tenant, policy_id = %row.policy_id, %error, "skipping an unparseable Cedar row")
            }
        }
    }
    // Every previous statement goes, kept ones too: the bundle adds them back
    // under their current labels, so no copy under an old label stays live.
    superseded.statements = previous
        .into_iter()
        .filter(|statement| !held_by_others.contains(statement))
        .collect();
    debug_assert!(superseded.statements.is_disjoint(&held_by_others));
    Ok(superseded)
}

fn parsed_statements(source: &str, cedar_text: &str) -> Result<BTreeSet<StatementKey>, String> {
    statement_keys(cedar_text)
        .map_err(|error| format!("Failed to parse Cedar in '{source}': {error}"))
}

/// Remove the superseded statements from the `primary` snapshot, which boot
/// loads in place of the legacy aggregate.
///
/// Runs before anything else is written, so a failure leaves the previous
/// install fully in place. The write is conditional on the snapshot read
/// earlier: a `PUT` that replaced it meanwhile wins, and the install stops.
///
/// # Errors
///
/// Returns an error when the snapshot cannot be parsed or written, or a
/// conflict when it changed since it was read.
pub(super) async fn rewrite_primary_snapshot(
    state: &PlatformState,
    tenant: &str,
    superseded: &SupersededAppPolicies,
) -> Result<(), String> {
    let (Some(policy_store), Some(primary)) = (state.server.policy_store(), &superseded.primary)
    else {
        return Ok(());
    };
    let stripped =
        remove_statements(&primary.cedar_text, &superseded.statements).map_err(|error| {
            format!("Failed to parse the '{PRIMARY_POLICY_ID}' policy row: {error}")
        })?;
    if stripped == primary.cedar_text {
        return Ok(());
    }
    let replaced = policy_store
        .replace_policy_if_hash(
            tenant,
            PRIMARY_POLICY_ID,
            &primary.policy_hash,
            &stripped,
            &primary.created_by,
        )
        .await
        .map_err(|error| {
            format!("Failed to rewrite the '{PRIMARY_POLICY_ID}' policy row: {error}")
        })?;
    if !replaced {
        return Err(format!(
            "Conflict: the '{PRIMARY_POLICY_ID}' policy row changed during the install; \
             nothing was written, install again against the current policies"
        ));
    }
    Ok(())
}
