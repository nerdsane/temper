use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

pub(super) async fn list_apps(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    app_name: &str,
) -> Option<Vec<String>> {
    match state.server.list_entity_ids_lazy(tenant_id, "App").await {
        Ok(ids) => Some(ids),
        Err(error) => {
            tracing::error!(
                tenant,
                app = %app_name,
                error = %error,
                "failed to enumerate App entities during bootstrap"
            );
            None
        }
    }
}
