use std::sync::Arc;

use crate::{
    AuthorizedWasmHost, SimWasmHost, WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate, WasmHost,
};

struct DenyAllGate;

impl WasmAuthzGate for DenyAllGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }

    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Deny("denied by policy".into())
    }
}

#[tokio::test]
async fn application_data_capability_delegates_without_http_authorization() {
    let inner = Arc::new(
        SimWasmHost::new()
            .with_data_budgets(123, 4)
            .with_data_response(b"bound-response".to_vec()),
    );
    let host = AuthorizedWasmHost::new(
        inner,
        Arc::new(DenyAllGate),
        WasmAuthzContext::test_fixture(),
    );

    assert_eq!(host.temper_data_request_budget(), 123);
    assert_eq!(host.temper_data_response_handle_budget(), 4);
    assert_eq!(
        host.temper_data_call(b"request").await,
        Ok(b"bound-response".to_vec())
    );
}
