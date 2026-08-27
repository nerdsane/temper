use temper_runtime::tenant::TenantId;
use temper_wasm_sdk::schema_deployment::StreamDescriptorMigrationTargetV1;

use super::super::{AppBundle, OsAppBundleDigest, OsAppReconcileResult};
use crate::state::PlatformState;

pub(super) enum Gate {
    Ready {
        capability_digest: Option<String>,
        fence_already_active: bool,
    },
    MigrationRequired(OsAppReconcileResult),
}

pub(super) async fn gate(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    digest: &OsAppBundleDigest,
    bundle: &AppBundle,
) -> Result<Gate, String> {
    let Some(csdl) = bundle.csdl.as_deref() else {
        return Ok(Gate::Ready {
            capability_digest: None,
            fence_already_active: false,
        });
    };
    let Some(capability_digest) = state
        .server
        .stage_installed_application_stream_migration_target_v1(
            &TenantId::new(tenant),
            app_name,
            &digest.spec_digest,
            csdl,
            &bundle.specs,
        )
        .await?
    else {
        return Ok(Gate::Ready {
            capability_digest: None,
            fence_already_active: false,
        });
    };
    if state
        .server
        .installed_application_stream_contract_activated_v1(
            &TenantId::new(tenant),
            app_name,
            &digest.spec_digest,
            csdl,
        )
        .await?
    {
        return Ok(Gate::Ready {
            capability_digest: Some(capability_digest),
            fence_already_active: true,
        });
    }
    let target = StreamDescriptorMigrationTargetV1::InstalledApplication {
        application_id: app_name.into(),
        semantic_digest: digest.spec_digest.clone(),
    };
    match state
        .server
        .require_stream_descriptor_completion_v1(&TenantId::new(tenant), &target, None)
        .await
    {
        Ok(_) => {}
        Err(error)
            if error.starts_with("backend unavailable:") || error.starts_with("stale fence:") =>
        {
            return Err(error);
        }
        Err(_) => {
            return Ok(Gate::MigrationRequired(
                OsAppReconcileResult::MigrationRequired {
                    app_name: app_name.into(),
                    semantic_digest: digest.spec_digest.clone(),
                    capability_digest,
                    descriptor_contract_version: 1,
                },
            ));
        }
    }
    Ok(Gate::Ready {
        capability_digest: Some(capability_digest),
        fence_already_active: false,
    })
}
