//! Versioned application-data and File stream WASM imports.

use wasmtime::{Caller, Linker};

use super::host_functions::{read_guest_bytes, run_host_call_with_timeout};
use super::{HostState, WasmError};

pub(super) fn link(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    linker
        .func_wrap(
            "env",
            "host_temper_data_call",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> i64 {
                if len <= 0 {
                    return -1;
                }
                if len as usize > caller.data().host.temper_data_request_budget() {
                    return -2;
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return -1;
                };
                let Ok(request) = read_guest_bytes(
                    &caller,
                    &memory,
                    ptr,
                    len,
                    "host_temper_data_call",
                    "request",
                ) else {
                    return -1;
                };
                if caller.data().data_responses.len()
                    >= caller.data().host.temper_data_response_handle_budget()
                {
                    return -3;
                }
                let host = caller.data().host.clone();
                let deadline = caller.data().remaining_host_call_timeout();
                let Ok(result) =
                    run_host_call_with_timeout("host_temper_data_call", deadline, async move {
                        host.temper_data_call(&request).await
                    })
                else {
                    return -4;
                };
                let Ok(response) = result else { return -4 };
                let handle = caller.data().next_data_response;
                let Some(next) = handle.checked_add(1) else {
                    return -4;
                };
                caller.data_mut().next_data_response = next;
                caller.data_mut().data_responses.insert(handle, response);
                i64::from(handle)
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link host_temper_data_call: {error}"))
        })?;

    linker
        .func_wrap(
            "env",
            "host_temper_data_response_len",
            |caller: Caller<'_, HostState>, handle: i32| -> i32 {
                caller
                    .data()
                    .data_responses
                    .get(&handle)
                    .and_then(|bytes| i32::try_from(bytes.len()).ok())
                    .unwrap_or(-1)
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link data response len: {error}"))
        })?;

    linker
        .func_wrap(
            "env",
            "host_temper_data_response_read",
            |mut caller: Caller<'_, HostState>,
             handle: i32,
             offset: i32,
             ptr: i32,
             len: i32|
             -> i32 {
                if offset < 0 || ptr < 0 || len < 0 {
                    return -1;
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return -1;
                };
                let Some(response) = caller.data().data_responses.get(&handle) else {
                    return -1;
                };
                let start = offset as usize;
                if start == response.len() {
                    return 0;
                }
                let Some(end) = start
                    .checked_add(len as usize)
                    .map(|end| end.min(response.len()))
                else {
                    return -1;
                };
                if start > response.len() {
                    return -1;
                }
                let bytes = response[start..end].to_vec();
                if memory.write(&mut caller, ptr as usize, &bytes).is_err() {
                    return -1;
                }
                i32::try_from(bytes.len()).unwrap_or(-1)
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link data response read: {error}"))
        })?;

    linker
        .func_wrap(
            "env",
            "host_temper_data_response_close",
            |mut caller: Caller<'_, HostState>, handle: i32| -> i32 {
                if caller.data_mut().data_responses.remove(&handle).is_some() {
                    0
                } else {
                    -1
                }
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link data response close: {error}"))
        })?;

    linker
        .func_wrap(
            "env",
            "host_temper_file_stream_read",
            |mut caller: Caller<'_, HostState>, handle: i32, ptr: i32, len: i32| -> i32 {
                if handle <= 0 || ptr < 0 || len < 0 {
                    return -3;
                }
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return -4;
                };
                match caller
                    .data()
                    .host
                    .temper_file_stream_read(handle as u32, len as usize)
                {
                    Ok(bytes) if bytes.len() <= len as usize => {
                        if memory.write(&mut caller, ptr as usize, &bytes).is_err() {
                            -4
                        } else {
                            i32::try_from(bytes.len()).unwrap_or(-4)
                        }
                    }
                    Ok(_) => -4,
                    Err(code) => code,
                }
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link File stream read: {error}"))
        })?;

    linker
        .func_wrap(
            "env",
            "host_temper_file_stream_try_write",
            |mut caller: Caller<'_, HostState>, handle: i32, ptr: i32, len: i32| -> i32 {
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(|export| export.into_memory())
                else {
                    return -4;
                };
                let Ok(bytes) = read_guest_bytes(
                    &caller,
                    &memory,
                    ptr,
                    len,
                    "host_temper_file_stream_try_write",
                    "chunk",
                ) else {
                    return -4;
                };
                match caller
                    .data()
                    .host
                    .temper_file_stream_try_write(handle as u32, &bytes)
                {
                    Ok(written) => i32::try_from(written).unwrap_or(-4),
                    Err(code) => code,
                }
            },
        )
        .map_err(|error| {
            WasmError::Compilation(format!("failed to link File stream write: {error}"))
        })?;

    Ok(())
}
