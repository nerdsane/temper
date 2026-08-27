//! Invocation-scoped File stream handles with explicit commit and abort.

use std::collections::BTreeMap;

use temper_wasm_sdk::data::{
    CommitToken, DataOperationKind, DataResultV1, FileMetadataV1, FileOperationKind,
    ModuleDataBudgets, ModuleDataError, ModuleDataErrorKind,
};

use crate::request_context::AgentContext;

use super::{ApplicationDataInvocation, data_error};

#[path = "streams/descriptor_error.rs"]
mod descriptor_error;
use descriptor_error::{invalid_stream, stream_descriptor_error, stream_registry_unavailable};

enum FileStream {
    Read {
        offset: usize,
    },
    Write {
        entity_type: String,
        file_id: String,
        expected_length: Option<u64>,
        expected_hash: Option<String>,
        expected_sequence: Option<u64>,
        committing: bool,
    },
}

#[derive(Debug)]
struct FileCommitAttempt {
    entity_type: String,
    file_id: String,
    expected_sequence: Option<u64>,
    bytes: Vec<u8>,
}

pub(super) struct FileStreamRegistry {
    next_handle: u32,
    streams: BTreeMap<u32, FileStream>,
    max_open: usize,
    max_bytes: u64,
    buffers: temper_wasm::StreamRegistry,
}

impl FileStreamRegistry {
    pub(super) fn new(budgets: &ModuleDataBudgets) -> Self {
        Self {
            next_handle: 1,
            streams: BTreeMap::new(),
            max_open: budgets.max_open_streams as usize,
            max_bytes: budgets.max_stream_bytes,
            buffers: temper_wasm::StreamRegistry::new(),
        }
    }

    fn insert(&mut self, stream: FileStream, bytes: Vec<u8>) -> Result<u32, ModuleDataError> {
        if self.streams.len() >= self.max_open {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "OpenStreamBudgetExceeded",
                "File stream budget exhausted",
            ));
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or_else(|| {
            data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "StreamHandleExhausted",
                "File stream handles exhausted",
            )
        })?;
        self.buffers.register_stream(&handle.to_string(), bytes);
        self.streams.insert(handle, stream);
        Ok(handle)
    }

    fn read(&mut self, handle: u32, max: usize) -> Result<Vec<u8>, i32> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let FileStream::Read { offset } = self.streams.get(&handle).ok_or(-3)? else {
            return Err(-3);
        };
        let offset = *offset;
        let stream_id = handle.to_string();
        let bytes = self.buffers.get_stream(&stream_id).ok_or(-3)?;
        if offset == bytes.len() {
            self.take(handle);
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(max).min(bytes.len());
        if end as u64 > self.max_bytes {
            return Err(-4);
        }
        let chunk = bytes[offset..end].to_vec();
        if let Some(FileStream::Read { offset }) = self.streams.get_mut(&handle) {
            *offset = end;
        }
        Ok(chunk)
    }

    fn write(&mut self, handle: u32, bytes: &[u8]) -> Result<usize, i32> {
        let FileStream::Write { committing, .. } = self.streams.get(&handle).ok_or(-3)? else {
            return Err(-3);
        };
        if *committing {
            return Err(-3);
        }
        self.buffers
            .append_stream_bounded(&handle.to_string(), bytes, self.max_bytes as usize)
            .ok_or(-4)
    }
}

impl ApplicationDataInvocation {
    pub(super) async fn file_read_open(
        &self,
        file_id: String,
        version_id: Option<String>,
    ) -> Result<DataResultV1, ModuleDataError> {
        let file_type = self.require_file(
            DataOperationKind::FileRead,
            if version_id.is_some() {
                FileOperationKind::VersionRead
            } else {
                FileOperationKind::ContentRead
            },
        )?;
        self.authorize("read", &file_type, Some(&file_id))?;
        let current_capability = self
            .authority
            .binding
            .stream_capabilities
            .iter()
            .find(|capability| capability.subject_type == file_type)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "StreamCapabilityUnavailable",
                    "Artifact is not bound to verified stream descriptor semantics",
                )
            })?;
        let current_runtime_type = current_capability
            .subject_type
            .rsplit('.')
            .next()
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "StreamCapabilityInvalid",
                    "Artifact stream subject type is invalid",
                )
            })?;
        if !self
            .state
            .stream_descriptor_contract_activated(
                &self.authority.tenant,
                None,
                current_runtime_type,
            )
            .await
            .map_err(stream_descriptor_error)?
        {
            return Err(data_error(
                ModuleDataErrorKind::ConsistencyUnavailable,
                "StreamDescriptorContractInactive",
                "Stream descriptor admission is not activated for this tenant schema",
            ));
        }
        let file_state = self
            .state
            .get_tenant_entity_state(&self.authority.tenant, current_runtime_type, &file_id)
            .await
            .map_err(super::internal_error)?;
        let (subject_type, subject_id) = if let Some(version_id) = &version_id {
            let version_type = current_capability
                .version_entity_type
                .as_deref()
                .and_then(|qualified| qualified.rsplit('.').next())
                .ok_or_else(|| {
                    data_error(
                        ModuleDataErrorKind::SchemaMismatch,
                        "VersionStreamCapabilityUnavailable",
                        "Artifact has no verified immutable version capability",
                    )
                })?;
            if !self
                .state
                .stream_descriptor_contract_activated(&self.authority.tenant, None, version_type)
                .await
                .map_err(stream_descriptor_error)?
            {
                return Err(data_error(
                    ModuleDataErrorKind::ConsistencyUnavailable,
                    "StreamDescriptorContractInactive",
                    "Version stream descriptor admission is not activated for this tenant schema",
                ));
            }
            (version_type, version_id.as_str())
        } else {
            (current_runtime_type, file_id.as_str())
        };
        let descriptor = self
            .state
            .resolve_stream_descriptor(&self.authority.tenant, subject_type, subject_id)
            .await
            .map_err(stream_descriptor_error)?;
        if version_id.is_some() {
            let parent = descriptor.authorization_parent();
            if descriptor.mutability() != temper_runtime::persistence::StreamMutability::Immutable
                || parent.is_none_or(|parent| {
                    parent.entity_type() != current_runtime_type || parent.entity_id() != file_id
                })
            {
                return Err(data_error(
                    ModuleDataErrorKind::InvalidRequest,
                    "FileVersionMismatch",
                    "File version does not belong to the requested File",
                ));
            }
        } else if descriptor.mutability() != temper_runtime::persistence::StreamMutability::Mutable
            || descriptor.authorization_parent().is_some()
        {
            return Err(data_error(
                ModuleDataErrorKind::ConsistencyUnavailable,
                "StreamDescriptorCapabilityMismatch",
                "Committed stream descriptor differs from verified schema semantics",
            ));
        }
        let bytes = self
            .state
            .read_stream_descriptor_bytes(
                &self.authority.tenant,
                &descriptor,
                self.authority.binding.grant.budgets.max_stream_bytes,
            )
            .await
            .map_err(stream_descriptor_error)?;
        let length = descriptor.byte_length();
        let content_hash = Some(descriptor.content_hash().to_string());
        let content_type = descriptor.content_type().map(str::to_string);
        let handle = self
            .streams
            .lock()
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::Internal,
                    "InvocationStatePoisoned",
                    "File stream registry unavailable",
                )
            })?
            .insert(FileStream::Read { offset: 0 }, bytes)?;
        Ok(DataResultV1::FileRead {
            stream_handle: handle,
            metadata: FileMetadataV1 {
                file_id,
                version_id,
                content_type,
                content_hash,
            },
            sequence: file_state.state.sequence_nr,
            content_length: Some(length),
        })
    }

    pub(super) async fn file_write_open(
        &self,
        file_id: String,
        expected: Option<u64>,
        expected_length: Option<u64>,
        expected_hash: Option<String>,
    ) -> Result<DataResultV1, ModuleDataError> {
        let file_type = self.require_file(
            DataOperationKind::FileWrite,
            FileOperationKind::ContentWrite,
        )?;
        self.authorize("update", &file_type, Some(&file_id))?;
        self.check_sequence(&file_type, &file_id, expected).await?;
        if expected_length
            .is_some_and(|length| length > self.authority.binding.grant.budgets.max_stream_bytes)
        {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "FileSizeBudgetExceeded",
                "declared File length exceeds stream budget",
            ));
        }
        let handle = self
            .streams
            .lock()
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::Internal,
                    "InvocationStatePoisoned",
                    "File stream registry unavailable",
                )
            })?
            .insert(
                FileStream::Write {
                    entity_type: file_type,
                    file_id,
                    expected_length,
                    expected_hash,
                    expected_sequence: expected,
                    committing: false,
                },
                Vec::new(),
            )?;
        Ok(DataResultV1::FileWrite {
            stream_handle: handle,
        })
    }

    pub(super) async fn file_write_commit(
        &self,
        handle: u32,
    ) -> Result<DataResultV1, ModuleDataError> {
        let attempt = self
            .streams
            .lock()
            .map_err(|_| stream_registry_unavailable())?
            .begin_commit(handle)?;
        let agent = AgentContext {
            security_ctx: Some(self.authority.security.clone()),
            agent_id: Some(self.authority.security.principal.id.clone()),
            expected_entity_sequence: attempt.expected_sequence,
            ..AgentContext::default()
        };
        let result = async {
            self.check_sequence(
                &attempt.entity_type,
                &attempt.file_id,
                attempt.expected_sequence,
            )
            .await?;
            self.state
                .put_file_stream_content(
                    &self.authority.tenant,
                    &attempt.file_id,
                    &attempt.bytes,
                    "application/octet-stream",
                    &agent,
                )
                .await
                .map_err(super::internal_error)
        }
        .await;
        let response = match result {
            Ok(response) => {
                self.streams
                    .lock()
                    .map_err(|_| stream_registry_unavailable())?
                    .finish_commit(handle, true)?;
                response
            }
            Err(error) => {
                self.streams
                    .lock()
                    .map_err(|_| stream_registry_unavailable())?
                    .finish_commit(handle, false)?;
                return Err(error);
            }
        };
        Ok(DataResultV1::FileCommitted {
            commit: CommitToken {
                entity_type: attempt.entity_type,
                entity_id: attempt.file_id.clone(),
                sequence: response.state.sequence_nr,
            },
            metadata: Some(FileMetadataV1 {
                file_id: attempt.file_id,
                version_id: None,
                content_type: Some("application/octet-stream".into()),
                content_hash: None,
            }),
        })
    }

    pub(super) fn file_abort(&self, handle: u32) -> Result<DataResultV1, ModuleDataError> {
        self.streams
            .lock()
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::Internal,
                    "InvocationStatePoisoned",
                    "File stream registry unavailable",
                )
            })?
            .take(handle)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::InvalidRequest,
                    "InvalidFileStream",
                    "File stream handle is invalid",
                )
            })?;
        Ok(DataResultV1::Aborted)
    }

    pub(super) fn stream_read(&self, handle: u32, max: usize) -> Result<Vec<u8>, i32> {
        self.streams.lock().map_err(|_| -4)?.read(handle, max)
    }

    pub(super) fn stream_write(&self, handle: u32, bytes: &[u8]) -> Result<usize, i32> {
        self.streams.lock().map_err(|_| -4)?.write(handle, bytes)
    }
}

impl FileStreamRegistry {
    fn take(&mut self, handle: u32) -> Option<(FileStream, Vec<u8>)> {
        let stream = self.streams.remove(&handle)?;
        let bytes = self
            .buffers
            .take_stream(&handle.to_string())
            .unwrap_or_default();
        Some((stream, bytes))
    }

    fn begin_commit(&mut self, handle: u32) -> Result<FileCommitAttempt, ModuleDataError> {
        let stream_id = handle.to_string();
        let bytes = self
            .buffers
            .get_stream(&stream_id)
            .ok_or_else(invalid_stream)?;
        let FileStream::Write {
            entity_type,
            file_id,
            expected_length,
            expected_hash,
            expected_sequence,
            committing,
        } = self.streams.get_mut(&handle).ok_or_else(invalid_stream)?
        else {
            return Err(invalid_stream());
        };
        if *committing {
            return Err(data_error(
                ModuleDataErrorKind::Conflict,
                "FileCommitInProgress",
                "File stream commit is already in progress",
            ));
        }
        if expected_length.is_some_and(|expected| expected != bytes.len() as u64) {
            return Err(data_error(
                ModuleDataErrorKind::Conflict,
                "FileLengthMismatch",
                "written File length does not match declaration",
            ));
        }
        if let Some(expected_hash) = expected_hash {
            use sha2::{Digest, Sha256};
            let actual = format!("sha256:{:x}", Sha256::digest(bytes));
            if &actual != expected_hash {
                return Err(data_error(
                    ModuleDataErrorKind::Conflict,
                    "FileHashMismatch",
                    "written File hash does not match declaration",
                ));
            }
        }
        *committing = true;
        Ok(FileCommitAttempt {
            entity_type: entity_type.clone(),
            file_id: file_id.clone(),
            expected_sequence: *expected_sequence,
            bytes: bytes.to_vec(),
        })
    }

    fn finish_commit(&mut self, handle: u32, committed: bool) -> Result<(), ModuleDataError> {
        let Some(FileStream::Write { committing, .. }) = self.streams.get_mut(&handle) else {
            return Err(invalid_stream());
        };
        if !*committing {
            return Err(invalid_stream());
        }
        if committed {
            self.take(handle);
        } else {
            *committing = false;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "streams_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "streams/restart_tests.rs"]
mod restart_tests;
