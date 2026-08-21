//! WASM sandboxed integration runtime for Temper.
//!
//! Provides a secure, resource-limited WASM execution environment for
//! agent-generated integration handlers. Modules are compiled once,
//! cached by SHA-256 hash, and invoked with per-call fuel and memory
//! budgets (TigerStyle).

pub mod authorized_host;
pub mod engine;
pub mod host_trait;
pub mod http_stream;
pub(crate) mod metrics;
pub mod stream;
pub mod types;
mod workflow_headers;

pub use authorized_host::{AuthorizedWasmHost, WasmAuthzDecision, WasmAuthzGate, extract_domain};
pub use engine::{PureMigrationError, PureMigrationLimits, WasmEngine, WasmError};
pub use host_trait::{
    BinaryHttpInterceptorFn, InternalHttpCapability, InternalHttpCapabilityIssuerFn,
    ProductionWasmHost, ProgressEmitterFn, SecretResolverFn, SimWasmHost, SpecEvaluatorFn,
    TemperDataCallFn, TemperFileReadFn, TemperFileWriteFn, TextHttpInterceptorFn, WasmHost,
    parse_connect_frames,
};
pub use stream::{StreamRegistry, StreamRegistryConfig};
pub use types::{
    WasmAuthzContext, WasmInvocationContext, WasmInvocationResult, WasmResourceLimits,
};
