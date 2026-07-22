use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{PlatformStore, SpecVerificationUpdate};
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_spec::csdl::parse_csdl;

use super::AppBundle;
use crate::state::PlatformState;

fn tenant_has_registered_app_specs_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if !tenant_has_app_spec_content_for_bundle(state, tenant, bundle) {
        return false;
    }
    if !tenant_has_app_spec_tables_for_bundle(state, tenant, bundle) {
        return false;
    }

    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    let Some(csdl_xml) = bundle.csdl.as_deref() else {
        return true;
    };
    let Ok(csdl) = parse_csdl(csdl_xml) else {
        return false;
    };

    for schema in &csdl.schemas {
        for container in &schema.entity_containers {
            for entity_set in &container.entity_sets {
                let expected_type = entity_set
                    .entity_type
                    .rsplit('.')
                    .next()
                    .unwrap_or(&entity_set.entity_type);
                if registry
                    .resolve_entity_type(&tenant_id, &entity_set.name)
                    .as_deref()
                    != Some(expected_type)
                {
                    return false;
                }
            }
        }
    }

    true
}

fn tenant_has_app_spec_content_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    bundle.specs.iter().all(|(entity_type, ioa_source)| {
        let Some(existing) = registry.get_spec(&tenant_id, entity_type) else {
            return false;
        };
        temper_store_turso::spec_content_hash(&existing.ioa_source)
            == temper_store_turso::spec_content_hash(ioa_source)
    })
}

fn tenant_has_app_spec_tables_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    bundle
        .specs
        .iter()
        .all(|(entity_type, _)| registry.get_table(&tenant_id, entity_type).is_some())
}

async fn repair_app_runtime_metadata_from_bundle(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<(), String> {
    if bundle.specs.is_empty() {
        return Ok(());
    }

    let Some(csdl_xml) = bundle.csdl.as_deref() else {
        return Ok(());
    };
    let csdl = parse_csdl(csdl_xml)
        .map_err(|error| format!("Failed to parse CSDL for os-app '{app_name}': {error}"))?;
    let specs: Vec<(&str, &str)> = bundle
        .specs
        .iter()
        .map(|(entity_type, ioa_source)| (entity_type.as_str(), ioa_source.as_str()))
        .collect();
    let tenant_id = TenantId::new(tenant);
    let target_digest = super::reconcile::digest_app_bundle(app_name, bundle);
    let publication_intent = temper_server::ServerState::spec_publication_intent(
        "os-app-bundle",
        [("bundle", target_digest.bundle_digest.as_bytes())],
    );
    let mut publication_guard = state.server.begin_spec_publication(&tenant_id).await?;
    if !tenant_has_app_spec_content_for_bundle(state, tenant, bundle)
        || !tenant_has_app_spec_tables_for_bundle(state, tenant, bundle)
    {
        return Err(format!(
            "OS app '{app_name}' spec generation changed while runtime repair waited"
        ));
    }
    state
        .server
        .arm_spec_publication(&mut publication_guard, &tenant_id, &publication_intent)?;
    // The matching bundle digest proves these specs are already durable. Fence
    // their exact key contracts before restoring the missing live metadata.
    let mut cutover = state
        .server
        .prepare_key_index_contracts_for_spec_activation(&publication_guard, &tenant_id, &specs)
        .await?;

    {
        let mut registry = state.registry.write().expect("Spec registry lock poisoned");
        registry
            .try_register_tenant_with_reactions_constraints_and_key_epochs(
                tenant_id.clone(),
                csdl,
                csdl_xml.to_string(),
                &specs,
                Vec::new(),
                bundle.cross_invariants_toml.clone(),
                true,
                &cutover.activation_epochs,
            )
            .map_err(|error| {
                format!("Failed to restore runtime metadata for os-app '{app_name}': {error}")
            })?;
    }
    state
        .server
        .finish_key_index_contract_activation(&mut publication_guard, &tenant_id, &mut cutover)
        .await?;
    state.server.rebuild_reaction_dispatcher();
    state
        .server
        .complete_spec_publication(&mut publication_guard, &tenant_id)?;
    tracing::info!(
        tenant,
        app = %app_name,
        "Restored OS app runtime metadata from matching bundle digest"
    );
    Ok(())
}

pub(crate) fn tenant_has_ready_app_specs_for_bundle(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
) -> bool {
    if !tenant_has_registered_app_specs_for_bundle(state, tenant, bundle) {
        return false;
    }

    let tenant_id = TenantId::new(tenant);
    let registry = state.registry.read().expect("Spec registry lock poisoned");
    bundle.specs.iter().all(|(entity_type, _)| {
        matches!(
            registry.get_verification_status(&tenant_id, entity_type),
            Some(VerificationStatus::Completed(_) | VerificationStatus::Restored(_))
        )
    })
}

async fn mark_app_specs_restored_from_matching_digest(
    state: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) {
    if bundle.specs.is_empty() {
        return;
    }

    let tenant_id = TenantId::new(tenant);
    let verified_at = temper_runtime::scheduler::sim_now().to_rfc3339();
    let result = EntityVerificationResult {
        all_passed: true,
        levels: vec![EntityLevelSummary {
            level: "BundleDigest".to_string(),
            passed: true,
            summary: format!("Restored from matching OS app bundle digest ({app_name})"),
            details: None,
        }],
        verified_at,
    };

    {
        let mut registry = state.registry.write().expect("Spec registry lock poisoned");
        for (entity_type, _) in &bundle.specs {
            registry.set_verification_status(
                &tenant_id,
                entity_type,
                VerificationStatus::Restored(result.clone()),
            );
        }
    }

    for (entity_type, _) in &bundle.specs {
        if let Err(error) = ps
            .persist_spec_verification(
                tenant,
                entity_type,
                SpecVerificationUpdate {
                    status: "passed",
                    verified: true,
                    levels_passed: Some(1),
                    levels_total: Some(1),
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!(
                tenant,
                app = %app_name,
                entity_type,
                error = %error,
                "Failed to persist restored OS app spec verification status"
            );
        }
    }
}

pub(crate) async fn restore_app_specs_from_matching_digest(
    state: &PlatformState,
    ps: &dyn PlatformStore,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> bool {
    if !tenant_has_app_spec_content_for_bundle(state, tenant, bundle) {
        return false;
    }

    if tenant_has_registered_app_specs_for_bundle(state, tenant, bundle) {
        mark_app_specs_restored_from_matching_digest(state, ps, tenant, app_name, bundle).await;
        return true;
    }

    if !tenant_has_app_spec_tables_for_bundle(state, tenant, bundle) {
        return false;
    }

    if let Err(error) =
        repair_app_runtime_metadata_from_bundle(state, tenant, app_name, bundle).await
    {
        tracing::warn!(
            tenant,
            app = %app_name,
            error = %error,
            "Failed to repair OS app runtime metadata from matching digest"
        );
        return false;
    }

    if tenant_has_registered_app_specs_for_bundle(state, tenant, bundle) {
        mark_app_specs_restored_from_matching_digest(state, ps, tenant, app_name, bundle).await;
        return true;
    }

    false
}
