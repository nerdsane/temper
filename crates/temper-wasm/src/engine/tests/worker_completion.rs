//! The actual invocation thread must distinguish delivered errors from loss.
use super::*;

struct PanickingHost;
#[async_trait]
impl WasmHost for PanickingHost {
    fn exports_llm_content(&self) -> bool {
        panic!("injected invocation worker termination")
    }
    async fn http_call(
        &self,
        _: &str,
        _: &str,
        _: &[(String, String)],
        _: &str,
    ) -> Result<(u16, String), String> {
        unreachable!()
    }
    async fn http_call_binary(
        &self,
        _: &str,
        _: &str,
        _: &[(String, String)],
        _: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        unreachable!()
    }
    fn get_secret(&self, _: &str) -> Result<String, String> {
        unreachable!()
    }
    fn log(&self, _: &str, _: &str) {
        unreachable!()
    }
}

#[tokio::test]
async fn delivered_invocation_error_is_not_worker_termination() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(WAT_TRAP.as_bytes()).unwrap();
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    assert!(
        matches!(result, Err(WasmError::Invocation(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn lost_invocation_worker_has_typed_unknown_completion() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(WAT_NOOP.as_bytes()).unwrap();
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            Arc::new(PanickingHost),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    assert!(
        matches!(result, Err(WasmError::WorkerTerminated)),
        "{result:?}"
    );
}
