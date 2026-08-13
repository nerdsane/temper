//! Construction of host-only invocation authority and callbacks.

use std::sync::{Arc, Mutex};

use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;
use temper_wasm::{TemperDataCallFn, TemperFileReadFn, TemperFileWriteFn};
use temper_wasm_sdk::data::ModuleSdkManifest;

use crate::state::ServerState;

use super::streams::FileStreamRegistry;

/// Host-only authority captured at a module invocation boundary.
#[derive(Clone)]
pub(crate) struct ModuleInvocationAuthority {
    pub(super) tenant: TenantId,
    pub(super) module_name: String,
    pub(super) artifact_digest: String,
    pub(super) trigger: String,
    pub(super) triggering_entity_type: String,
    pub(super) grant_digest: String,
    pub(super) security: SecurityContext,
    pub(super) binding: ModuleSdkManifest,
}

impl ModuleInvocationAuthority {
    pub(crate) fn new(
        tenant: TenantId,
        module_name: String,
        artifact_digest: String,
        trigger: String,
        triggering_entity_type: String,
        security: SecurityContext,
        binding: ModuleSdkManifest,
    ) -> Self {
        let grant_digest = binding.grant_digest.clone();
        Self {
            tenant,
            module_name,
            artifact_digest,
            trigger,
            triggering_entity_type,
            grant_digest,
            security,
            binding,
        }
    }
}

/// One invocation-scoped service and its bounded response/File resources.
pub(crate) struct ApplicationDataInvocation {
    pub(super) state: ServerState,
    pub(super) authority: ModuleInvocationAuthority,
    pub(super) streams: Mutex<FileStreamRegistry>,
    pub(super) calls: Mutex<u32>,
}

impl ApplicationDataInvocation {
    pub(crate) fn callbacks(
        self: &Arc<Self>,
    ) -> (TemperDataCallFn, TemperFileReadFn, TemperFileWriteFn) {
        let data_service = Arc::clone(self);
        let data: TemperDataCallFn = Arc::new(move |bytes| {
            let service = Arc::clone(&data_service);
            Box::pin(async move { service.call_encoded(&bytes).await })
        });
        let read_service = Arc::clone(self);
        let read: TemperFileReadFn =
            Arc::new(move |handle, max| read_service.stream_read(handle, max));
        let write_service = Arc::clone(self);
        let write: TemperFileWriteFn =
            Arc::new(move |handle, bytes| write_service.stream_write(handle, &bytes));
        (data, read, write)
    }

    pub(crate) fn new(state: ServerState, authority: ModuleInvocationAuthority) -> Arc<Self> {
        Arc::new(Self {
            state,
            streams: Mutex::new(FileStreamRegistry::new(&authority.binding.grant.budgets)),
            authority,
            calls: Mutex::new(0),
        })
    }
}
