//! Guest-side wrappers over the raw application-data ABI.

use std::collections::BTreeMap;

use super::{
    CommitToken, DataOperationV1, DataOutcomeV1, DataRequestV1, DataResponseV1, DataResultV1,
    FileMetadataV1, ModuleDataError, ModuleDataErrorKind, Retryability,
};

/// A decoded generated entity value and the sequence it represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedEntity<T> {
    /// Generated entity value.
    pub value: T,
    /// Durable entity sequence represented by the value.
    pub sequence: u64,
}

/// A decoded generated query page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedPage<T> {
    /// Generated entity values with their durable sequences.
    pub values: Vec<TypedEntity<T>>,
    /// Opaque cursor for the next stable page.
    pub next_cursor: Option<String>,
}

/// A decoded generated write acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedWrite<T> {
    /// Post-commit consistency token.
    pub commit: CommitToken,
    /// Written entity when it fit the response budget.
    pub value: Option<T>,
    /// Whether the committed value was intentionally omitted.
    pub value_omitted: bool,
}

/// A decoded generated action acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedAction<T> {
    /// Post-commit consistency token.
    pub commit: CommitToken,
    /// Typed action result when it fit the response budget.
    pub result: Option<T>,
    /// Whether the committed result was intentionally omitted.
    pub result_omitted: bool,
}

/// An opened File read and its metadata.
#[derive(Debug)]
pub struct OpenedFileRead {
    /// Invocation-scoped content reader.
    pub reader: FileReader,
    /// Metadata returned separately from content bytes.
    pub metadata: FileMetadataV1,
    /// File sequence represented by this read.
    pub sequence: u64,
    /// Declared content length when known.
    pub content_length: Option<u64>,
}

/// Decode an entity result into a generated entity type.
pub fn decode_entity<T: serde::de::DeserializeOwned>(
    result: DataResultV1,
) -> Result<TypedEntity<T>, ModuleDataError> {
    let DataResultV1::Entity { value, sequence } = result else {
        return Err(result_shape_error("Entity"));
    };
    Ok(TypedEntity {
        value: decode_object(value)?,
        sequence,
    })
}

/// Decode a page result into a generated entity type.
pub fn decode_page<T: serde::de::DeserializeOwned>(
    result: DataResultV1,
) -> Result<TypedPage<T>, ModuleDataError> {
    let DataResultV1::Page {
        values,
        next_cursor,
    } = result
    else {
        return Err(result_shape_error("Page"));
    };
    let values = values
        .into_iter()
        .map(|item| {
            Ok(TypedEntity {
                value: decode_object(item.value)?,
                sequence: item.sequence,
            })
        })
        .collect::<Result<Vec<_>, ModuleDataError>>()?;
    Ok(TypedPage {
        values,
        next_cursor,
    })
}

/// Decode a write result into a generated entity type.
pub fn decode_write<T: serde::de::DeserializeOwned>(
    result: DataResultV1,
) -> Result<TypedWrite<T>, ModuleDataError> {
    let DataResultV1::Write {
        commit,
        value,
        value_omitted,
    } = result
    else {
        return Err(result_shape_error("Write"));
    };
    Ok(TypedWrite {
        commit,
        value: value.map(decode_object).transpose()?,
        value_omitted,
    })
}

/// Decode an action result into its generated result type.
pub fn decode_action<T: serde::de::DeserializeOwned>(
    result: DataResultV1,
) -> Result<TypedAction<T>, ModuleDataError> {
    let DataResultV1::Action {
        commit,
        result,
        result_omitted,
    } = result
    else {
        return Err(result_shape_error("Action"));
    };
    Ok(TypedAction {
        commit,
        result: result.map(decode_json).transpose()?,
        result_omitted,
    })
}

/// Decode a File read-open result into its typed guest wrapper.
pub fn decode_file_read(result: DataResultV1) -> Result<OpenedFileRead, ModuleDataError> {
    let DataResultV1::FileRead {
        stream_handle,
        metadata,
        sequence,
        content_length,
    } = result
    else {
        return Err(result_shape_error("FileRead"));
    };
    Ok(OpenedFileRead {
        reader: DataClient::file_reader(stream_handle),
        metadata,
        sequence,
        content_length,
    })
}

/// Decode a File write-open result into its typed guest wrapper.
pub fn decode_file_write(result: DataResultV1) -> Result<FileWriter, ModuleDataError> {
    let DataResultV1::FileWrite { stream_handle } = result else {
        return Err(result_shape_error("FileWrite"));
    };
    Ok(DataClient::file_writer(stream_handle))
}

fn decode_object<T: serde::de::DeserializeOwned>(
    value: serde_json::Map<String, serde_json::Value>,
) -> Result<T, ModuleDataError> {
    serde_json::from_value(serde_json::Value::Object(value)).map_err(|error| {
        ModuleDataError::new(
            ModuleDataErrorKind::SchemaMismatch,
            "GeneratedResultTypeMismatch",
            error.to_string(),
            Retryability::Never,
        )
    })
}

fn decode_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, ModuleDataError> {
    serde_json::from_value(value).map_err(|error| {
        ModuleDataError::new(
            ModuleDataErrorKind::SchemaMismatch,
            "GeneratedResultTypeMismatch",
            error.to_string(),
            Retryability::Never,
        )
    })
}

fn result_shape_error(expected: &str) -> ModuleDataError {
    ModuleDataError::new(
        ModuleDataErrorKind::Internal,
        "UnexpectedDataResult",
        format!("host returned a result other than {expected}"),
        Retryability::Never,
    )
}

/// Typed entry point used by generated module clients.
#[derive(Debug, Clone, Default)]
pub struct DataClient {
    observed: BTreeMap<(String, String), u64>,
}

impl DataClient {
    /// Execute one operation and decode its structured result.
    pub fn call(
        &mut self,
        mut operation: DataOperationV1,
    ) -> Result<DataResultV1, ModuleDataError> {
        self.apply_observed_sequence(&mut operation);
        let request = DataRequestV1::new(operation);
        let bytes = serde_json::to_vec(&request).map_err(|error| {
            sdk_error(
                "RequestEncodingFailed",
                format!("failed to encode request: {error}"),
            )
        })?;
        let response = call_host(&bytes)?;
        if response.abi != super::DATA_ABI_VERSION_V1 {
            return Err(sdk_error(
                "ResponseAbiMismatch",
                format!("host returned unsupported ABI {}", response.abi),
            ));
        }
        match response.outcome {
            DataOutcomeV1::Ok { result } => {
                self.observe_result(&result);
                Ok(result)
            }
            DataOutcomeV1::Error { error } => Err(error),
        }
    }

    fn apply_observed_sequence(&self, operation: &mut DataOperationV1) {
        if let DataOperationV1::EntityGet {
            entity_type,
            entity_id,
            at_least_sequence,
        } = operation
            && let Some(observed) = self.observed.get(&(entity_type.clone(), entity_id.clone()))
        {
            *at_least_sequence = Some(at_least_sequence.unwrap_or(0).max(*observed));
        }
    }

    /// Record an explicit durable commit token for a later keyed read.
    pub fn observe_commit(&mut self, token: &CommitToken) {
        let sequence = self
            .observed
            .entry((token.entity_type.clone(), token.entity_id.clone()))
            .or_default();
        *sequence = (*sequence).max(token.sequence);
    }

    fn observe_result(&mut self, result: &DataResultV1) {
        match result {
            DataResultV1::Write { commit, .. }
            | DataResultV1::Action { commit, .. }
            | DataResultV1::FileCommitted { commit, .. } => self.observe_commit(commit),
            DataResultV1::Batch { outcomes } => {
                for outcome in outcomes {
                    if let DataOutcomeV1::Ok { result } = outcome {
                        self.observe_result(result);
                    }
                }
            }
            _ => {}
        }
    }

    /// Wrap a File read handle returned by [`Self::call`].
    pub const fn file_reader(stream_handle: u32) -> FileReader {
        FileReader {
            stream_handle,
            consumed: false,
        }
    }

    /// Wrap a File write handle returned by [`Self::call`].
    pub const fn file_writer(stream_handle: u32) -> FileWriter {
        FileWriter {
            stream_handle,
            consumed: false,
        }
    }
}

/// Invocation-scoped File content reader.
#[derive(Debug)]
pub struct FileReader {
    stream_handle: u32,
    consumed: bool,
}

impl FileReader {
    /// Read one bounded chunk. `Ok(0)` is EOF and consumes the handle.
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, ModuleDataError> {
        if self.consumed {
            return Err(stream_error(
                "FileStreamConsumed",
                "File stream is consumed",
            ));
        }
        let result = file_stream_read(self.stream_handle, buffer);
        match result {
            Ok(0) => {
                self.consumed = true;
                Ok(0)
            }
            other => other,
        }
    }

    /// Opaque handle used by commit or abort data operations.
    pub const fn handle(&self) -> u32 {
        self.stream_handle
    }
}

/// Invocation-scoped File content writer.
#[derive(Debug)]
pub struct FileWriter {
    stream_handle: u32,
    consumed: bool,
}

impl FileWriter {
    /// Try to write one bounded chunk.
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<usize, ModuleDataError> {
        if self.consumed {
            return Err(stream_error(
                "FileStreamConsumed",
                "File stream is consumed",
            ));
        }
        file_stream_write(self.stream_handle, bytes)
    }

    /// Opaque handle used by commit or abort data operations.
    pub const fn handle(&self) -> u32 {
        self.stream_handle
    }

    /// Prevent further writes after a successful commit or abort.
    pub fn mark_consumed(&mut self) {
        self.consumed = true;
    }
}

fn sdk_error(code: &str, message: String) -> ModuleDataError {
    ModuleDataError::new(
        ModuleDataErrorKind::Internal,
        code,
        message,
        Retryability::Never,
    )
}

fn stream_error(code: &str, message: &str) -> ModuleDataError {
    ModuleDataError::new(
        ModuleDataErrorKind::InvalidRequest,
        code,
        message,
        Retryability::Never,
    )
}

#[cfg(target_arch = "wasm32")]
fn call_host(request: &[u8]) -> Result<DataResponseV1, ModuleDataError> {
    let handle = unsafe {
        crate::host::host_temper_data_call(request.as_ptr() as i32, request.len() as i32)
    };
    if handle <= 0 || handle > i32::MAX as i64 {
        return Err(sdk_error(
            "DataHostCallFailed",
            format!("data host returned ABI code {handle}"),
        ));
    }
    let handle = handle as i32;
    let len = unsafe { crate::host::host_temper_data_response_len(handle) };
    if len < 0 {
        return Err(sdk_error(
            "InvalidResponseHandle",
            "data host returned an invalid response handle".into(),
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    let read = unsafe {
        crate::host::host_temper_data_response_read(handle, 0, bytes.as_mut_ptr() as i32, len)
    };
    let close = unsafe { crate::host::host_temper_data_response_close(handle) };
    if read != len || close != 0 {
        return Err(sdk_error(
            "ResponseReadFailed",
            "failed to read or close data response".into(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        sdk_error(
            "ResponseDecodingFailed",
            format!("failed to decode response: {error}"),
        )
    })
}

#[cfg(all(not(target_arch = "wasm32"), feature = "test-helpers"))]
fn call_host(request: &[u8]) -> Result<DataResponseV1, ModuleDataError> {
    super::test_host::call(request)
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "test-helpers")))]
fn call_host(_request: &[u8]) -> Result<DataResponseV1, ModuleDataError> {
    Err(sdk_error(
        "HostUnavailable",
        "application-data host is only available on wasm32".into(),
    ))
}

#[cfg(target_arch = "wasm32")]
fn file_stream_read(handle: u32, buffer: &mut [u8]) -> Result<usize, ModuleDataError> {
    let result = unsafe {
        crate::host::host_temper_file_stream_read(
            handle as i32,
            buffer.as_mut_ptr() as i32,
            buffer.len() as i32,
        )
    };
    decode_stream_result(result)
}

#[cfg(not(target_arch = "wasm32"))]
fn file_stream_read(_handle: u32, _buffer: &mut [u8]) -> Result<usize, ModuleDataError> {
    Err(sdk_error(
        "HostUnavailable",
        "File stream host is only available on wasm32".into(),
    ))
}

#[cfg(target_arch = "wasm32")]
fn file_stream_write(handle: u32, bytes: &[u8]) -> Result<usize, ModuleDataError> {
    let result = unsafe {
        crate::host::host_temper_file_stream_try_write(
            handle as i32,
            bytes.as_ptr() as i32,
            bytes.len() as i32,
        )
    };
    decode_stream_result(result)
}

#[cfg(not(target_arch = "wasm32"))]
fn file_stream_write(_handle: u32, _bytes: &[u8]) -> Result<usize, ModuleDataError> {
    Err(sdk_error(
        "HostUnavailable",
        "File stream host is only available on wasm32".into(),
    ))
}

#[cfg(target_arch = "wasm32")]
fn decode_stream_result(result: i32) -> Result<usize, ModuleDataError> {
    match result {
        value if value >= 0 => Ok(value as usize),
        -1 => Err(ModuleDataError::new(
            ModuleDataErrorKind::TransientUnavailable,
            "WouldBlock",
            "File stream would block",
            Retryability::WithBackoff,
        )),
        -2 => Err(stream_error("FileStreamClosed", "File stream is closed")),
        -3 => Err(stream_error(
            "InvalidFileStream",
            "File stream handle is invalid",
        )),
        _ => Err(sdk_error(
            "FileStreamHostFailure",
            "File stream host failed".into(),
        )),
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
