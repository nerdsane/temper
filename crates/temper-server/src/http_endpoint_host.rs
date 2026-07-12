//! Governed WASM host construction for HttpEndpoint dispatch (ARN-208, ADR-0162).

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_wasm::http_stream::HttpStreamRegistry;
use temper_wasm::types::{WasmAuthzContext, WasmInvocationContext};
use temper_wasm::{AuthorizedWasmHost, ProductionWasmHost, WasmHost};

use crate::state::ServerState;

impl ServerState {
    /// Build the WASM host for an HttpEndpoint invocation.
    ///
    /// The `ProductionWasmHost` owns the shared inbound/outbound streams for the
    /// request/response bodies, and is wrapped in the governed `AuthorizedWasmHost`
    /// so secret access and outbound HTTP go through the same Cedar default-deny gate
    /// as entity-action WASM (ARN-208) — an HttpEndpoint module can no longer read
    /// tenant secrets outside policy. Body streaming stays ungated so the endpoint's
    /// own request/response handling is unchanged.
    pub(crate) fn build_http_endpoint_wasm_host(
        &self,
        ctx: &WasmInvocationContext,
        secrets: BTreeMap<String, String>,
        streams: Arc<HttpStreamRegistry>,
    ) -> Arc<dyn WasmHost> {
        let inner: Arc<dyn WasmHost> = Arc::new(
            ProductionWasmHost::with_shared_streams(secrets, streams)
                .with_invocation_context(ctx.clone()),
        );
        let authz_ctx = WasmAuthzContext {
            tenant: ctx.tenant.clone(),
            module_name: ctx.wasm_module.clone().unwrap_or_default(),
            agent_id: ctx.agent_id.clone(),
            session_id: ctx.session_id.clone(),
            entity_type: ctx.entity_type.clone(),
            trigger_action: ctx.trigger_action.clone(),
        };
        Arc::new(AuthorizedWasmHost::new(
            inner,
            self.wasm_authz_gate(),
            authz_ctx,
        ))
    }
}
