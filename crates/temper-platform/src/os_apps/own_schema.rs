//! The tenant schema an app install leaves behind.
//!
//! A tenant's schema is made of what its installed apps declare. An install
//! replaces the installing app's own contribution: for each schema namespace
//! the app declares, the namespace is rebuilt from that namespace in every
//! other installed app, then in the installing app, so what the app stopped
//! declaring leaves. Namespaces the app does not declare stay as they are.
//! An entity type whose spec is still registered stays declared (with its
//! entity set) even when no app declares it any more, so its records stay
//! readable.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{
    CsdlDocument, EntityContainer, Schema, emit_csdl_xml, merge_csdl, parse_csdl,
};

use super::entity_owners::installed_apps;
use super::types::InstallWarning;
use crate::state::PlatformState;

/// The schema a tenant holds once an app is installed.
pub(super) struct TenantSchema {
    /// The tenant's whole schema.
    pub(super) csdl_xml: String,
    /// What the install did to the schema that its owner should see.
    pub(super) warnings: Vec<InstallWarning>,
}

/// The tenant's schema after `app_name`, whose CSDL is `incoming_xml`, is
/// installed in `tenant`.
///
/// When the other installed apps cannot all be read (or the tenant has no
/// platform store to list them), nothing can be rebuilt without losing an
/// app's declarations, so the app's CSDL is merged into the schema as before
/// and, when apps were listed, a warning names those that could not be read.
pub(super) async fn for_install(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    incoming_xml: &str,
) -> Result<TenantSchema, String> {
    let incoming = parse_csdl(incoming_xml)
        .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))?;
    let (existing, registered) = {
        let registry = state.registry.read().expect("registry lock poisoned");
        match registry.get_tenant(&TenantId::new(tenant)) {
            Some(config) => (
                Some(config.csdl.clone()),
                config.entities.keys().cloned().collect::<BTreeSet<_>>(),
            ),
            None => (None, BTreeSet::new()),
        }
    };
    let Some(existing) = existing else {
        return Ok(TenantSchema {
            csdl_xml: incoming_xml.to_string(),
            warnings: Vec::new(),
        });
    };
    let installed = installed_apps(state, tenant, app_name).await?;
    let mut warnings = Vec::new();
    let mut contributions: Vec<&CsdlDocument> = Vec::new();
    let mut unreadable = Vec::new();
    for other in &installed.others {
        match &other.csdl {
            Ok(Some(csdl)) => contributions.push(csdl),
            Ok(None) => {}
            Err(why) => unreadable.push(format!("'{}' ({why})", other.name)),
        }
    }
    if !installed.listed || !unreadable.is_empty() {
        if !unreadable.is_empty() {
            warnings.push(InstallWarning::own_schema(format!(
                "the schema of '{app_name}' was merged into the tenant's, and nothing it no longer \
                 declares was removed, because these installed apps could not be read: {}",
                unreadable.join(", ")
            )));
        }
        return Ok(TenantSchema {
            csdl_xml: emit_csdl_xml(&merge_csdl(&existing, &incoming)),
            warnings,
        });
    }
    contributions.push(&incoming);
    let namespaces = incoming
        .schemas
        .iter()
        .map(|schema| schema.namespace.clone())
        .collect();
    let (csdl, kept) = rebuild(&existing, &contributions, &namespaces, &registered);
    warnings.extend(kept.into_iter().map(InstallWarning::own_schema));
    Ok(TenantSchema {
        csdl_xml: emit_csdl_xml(&csdl),
        warnings,
    })
}

/// `existing` with each of `namespaces` rebuilt from what `contributions`
/// declare in it, in order, and the lines saying what was kept: an entity
/// type in a rebuilt namespace that no schema declares any more but whose
/// spec is in `registered` keeps its declaration and entity sets.
pub(super) fn rebuild(
    existing: &CsdlDocument,
    contributions: &[&CsdlDocument],
    namespaces: &BTreeSet<String>,
    registered: &BTreeSet<String>,
) -> (CsdlDocument, Vec<String>) {
    let mut result = existing.clone();
    for namespace in namespaces {
        let mut built = CsdlDocument {
            version: existing.version.clone(),
            schemas: Vec::new(),
        };
        for contribution in contributions {
            let only = CsdlDocument {
                version: contribution.version.clone(),
                schemas: contribution
                    .schemas
                    .iter()
                    .filter(|schema| &schema.namespace == namespace)
                    .cloned()
                    .collect(),
            };
            built = merge_csdl(&built, &only);
        }
        let Some(schema) = built.schemas.into_iter().next() else {
            continue;
        };
        match result
            .schemas
            .iter_mut()
            .find(|s| &s.namespace == namespace)
        {
            Some(slot) => *slot = schema,
            None => result.schemas.push(schema),
        }
    }

    let mut declared: BTreeSet<String> = result
        .schemas
        .iter()
        .flat_map(|schema| &schema.entity_types)
        .map(|entity_type| entity_type.name.clone())
        .collect();
    let mut kept = Vec::new();
    for old in existing
        .schemas
        .iter()
        .filter(|schema| namespaces.contains(&schema.namespace))
    {
        for entity_type in &old.entity_types {
            if declared.contains(&entity_type.name) || !registered.contains(&entity_type.name) {
                continue;
            }
            let Some(slot) = result
                .schemas
                .iter_mut()
                .find(|schema| schema.namespace == old.namespace)
            else {
                continue;
            };
            slot.entity_types.push(entity_type.clone());
            let sets = keep_entity_sets(old, slot, &entity_type.name);
            declared.insert(entity_type.name.clone());
            kept.push(format!(
                "entity type {} (sets: {}) is no longer declared by any installed app; it is kept \
                 because its spec is registered in the tenant, so its records stay readable",
                entity_type.name,
                if sets.is_empty() {
                    "none".to_string()
                } else {
                    sets.join(", ")
                }
            ));
        }
    }
    (result, kept)
}

/// Copy the entity sets of `type_name` from `old` into the same-named
/// containers of `slot`; returns their names.
fn keep_entity_sets(old: &Schema, slot: &mut Schema, type_name: &str) -> Vec<String> {
    let mut names = Vec::new();
    for container in &old.entity_containers {
        let sets: Vec<_> = container
            .entity_sets
            .iter()
            .filter(|set| set.entity_type.rsplit('.').next() == Some(type_name))
            .cloned()
            .collect();
        if sets.is_empty() {
            continue;
        }
        names.extend(sets.iter().map(|set| set.name.clone()));
        match slot
            .entity_containers
            .iter_mut()
            .find(|c| c.name == container.name)
        {
            Some(target) => target.entity_sets.extend(sets),
            None => slot.entity_containers.push(EntityContainer {
                name: container.name.clone(),
                entity_sets: sets,
                action_imports: Vec::new(),
                function_imports: Vec::new(),
            }),
        }
    }
    names
}

/// After a restart, the registry holds the union of every stored spec row's
/// CSDL, and a row written before an app dropped a type still declares it.
/// Rebuild each tenant's schema the way the installs did: every namespace an
/// installed app declares is rebuilt from the installed apps, in the order
/// the store lists them. A tenant whose installed apps cannot all be read is
/// left as restored.
pub(crate) async fn converge_installed_schemas(
    state: &PlatformState,
    installed: &[(String, String)],
) {
    let tenants: BTreeSet<&str> = installed
        .iter()
        .map(|(tenant, _)| tenant.as_str())
        .collect();
    for tenant in tenants {
        let apps = match installed_apps(state, tenant, "").await {
            Ok(apps) if apps.listed => apps,
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(tenant, "schema not rebuilt after restart: {error}");
                continue;
            }
        };
        let mut contributions = Vec::new();
        for app in &apps.others {
            match &app.csdl {
                Ok(Some(csdl)) => contributions.push(csdl),
                Ok(None) => {}
                Err(why) => {
                    tracing::warn!(tenant, app = %app.name, "schema not rebuilt after restart: {why}");
                    contributions.clear();
                    break;
                }
            }
        }
        if contributions.is_empty() {
            continue;
        }
        let tenant_id = TenantId::new(tenant);
        let (existing, registered) = {
            let registry = state.registry.read().expect("registry lock poisoned");
            match registry.get_tenant(&tenant_id) {
                Some(config) => (
                    config.csdl.clone(),
                    config.entities.keys().cloned().collect::<BTreeSet<_>>(),
                ),
                None => continue,
            }
        };
        let namespaces = contributions
            .iter()
            .flat_map(|csdl| &csdl.schemas)
            .map(|schema| schema.namespace.clone())
            .collect();
        let (csdl, kept) = rebuild(&existing, &contributions, &namespaces, &registered);
        for line in kept {
            tracing::warn!(tenant, "{line}");
        }
        let xml = emit_csdl_xml(&csdl);
        if xml == emit_csdl_xml(&existing) {
            continue;
        }
        state
            .registry
            .write()
            .expect("registry lock poisoned")
            .set_tenant_schema(&tenant_id, csdl, xml);
    }
}
