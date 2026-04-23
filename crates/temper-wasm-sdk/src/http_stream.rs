//! Ergonomic Rust-side wrappers for the ADR-0057 streaming FFI.
//!
//! Guest code shouldn't deal with raw handle IDs and return-code
//! conventions. This module exposes:
//!
//!   * [`StreamHandle`] — newtype over `u32`, hides the tagged-i32
//!     semantics at the FFI boundary.
//!   * [`StreamError`] — rust-side enum mirroring the negative
//!     return codes from the host.
//!   * [`HttpRequestBodyWriter`] / [`HttpResponseBodyReader`] —
//!     thin wrappers around the handles with `write_all_chunk` /
//!     `read_next_chunk` semantics.
//!   * [`streaming_call`] — convenience that opens an outbound
//!     exchange, returns the two wrappers plus a closure to await
//!     the response head.
//!
//! Cooperative WouldBlock handling: writes spin on WouldBlock,
//! yielding control back to the host between attempts. WASM is
//! single-threaded inside a guest, so "yield" here means calling
//! `host_get_time_millis` as a scheduling hint. In practice the
//! host's bounded mpsc will empty quickly once the reader side is
//! pumping, so tight spins are bounded in wall time.

#![cfg(target_arch = "wasm32")]

use crate::host;

/// One end of a streaming channel owned by the host. Guest code
/// uses the handle to read or write through host functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHandle(pub u32);

/// Errors surfaced by the streaming wrappers. Maps 1:1 to the
/// host's [`StreamError`](../../temper-wasm/src/http_stream.rs) plus
/// a `Busy` variant that callers will have already retried through
/// if they used `write_all_chunk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    Closed,
    InvalidHandle,
    Other(String),
}

impl core::fmt::Display for StreamError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StreamError::Closed => write!(f, "stream closed"),
            StreamError::InvalidHandle => write!(f, "invalid stream handle"),
            StreamError::Other(msg) => write!(f, "stream error: {msg}"),
        }
    }
}

fn classify_rc(rc: i32) -> Result<usize, StreamError> {
    if rc >= 0 {
        Ok(rc as usize)
    } else {
        match rc {
            -2 => Err(StreamError::Closed),
            -3 => Err(StreamError::InvalidHandle),
            _ => Err(StreamError::Other(format!("host rc {rc}"))),
        }
    }
}

/// Write end of a streaming request body.
pub struct HttpRequestBodyWriter {
    handle: StreamHandle,
}

impl HttpRequestBodyWriter {
    pub fn handle(&self) -> StreamHandle {
        self.handle
    }

    /// Write `chunk` to the host, spinning cooperatively on
    /// WouldBlock. Returns number of bytes written on success.
    pub fn write_all_chunk(&mut self, chunk: &[u8]) -> Result<usize, StreamError> {
        if chunk.is_empty() {
            return Ok(0);
        }
        loop {
            let rc = unsafe {
                host::host_http_stream_try_write(
                    self.handle.0 as i32,
                    chunk.as_ptr() as i32,
                    chunk.len() as i32,
                )
            };
            if rc == -1 {
                // WouldBlock — yield and retry.
                unsafe {
                    let _ = host::host_get_time_millis();
                }
                continue;
            }
            return classify_rc(rc);
        }
    }

    /// Close the request body, signalling end-of-request to the
    /// host. After this, further writes return `Closed`.
    pub fn finish(self) -> Result<(), StreamError> {
        let rc = unsafe { host::host_http_stream_close(self.handle.0 as i32) };
        if rc == 0 {
            Ok(())
        } else if rc == -3 {
            Err(StreamError::InvalidHandle)
        } else {
            Err(StreamError::Other(format!("host rc {rc}")))
        }
    }
}

/// Read end of a streaming response body.
pub struct HttpResponseBodyReader {
    handle: StreamHandle,
}

impl HttpResponseBodyReader {
    pub fn handle(&self) -> StreamHandle {
        self.handle
    }

    /// Read the next chunk from the host. Returns `Ok(None)` on
    /// clean EOF, `Ok(Some(..))` otherwise.
    pub fn read_next_chunk(
        &mut self,
        buf: &mut [u8],
    ) -> Result<Option<usize>, StreamError> {
        let rc = unsafe {
            host::host_http_stream_read(
                self.handle.0 as i32,
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
            )
        };
        if rc == 0 {
            return Ok(None); // EOF
        }
        classify_rc(rc).map(Some)
    }

    /// Close the reader, freeing the handle.
    pub fn close(self) -> Result<(), StreamError> {
        let rc = unsafe { host::host_http_stream_close(self.handle.0 as i32) };
        if rc == 0 {
            Ok(())
        } else if rc == -3 {
            Err(StreamError::InvalidHandle)
        } else {
            Err(StreamError::Other(format!("host rc {rc}")))
        }
    }
}

/// Response head handed to the guest once the host has parsed
/// the HTTP response.
#[derive(Debug, Clone, Default)]
pub struct HttpResponseHead {
    pub status: u16,
    pub headers: alloc::vec::Vec<(alloc::string::String, alloc::string::String)>,
}

extern crate alloc;

/// Inbound HTTP dispatch context delivered through
/// `WasmInvocationContext.http_request`. Guest code deserializes
/// this from the context JSON and drives the inbound exchange via
/// its helpers.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct InboundHttp {
    pub method: alloc::string::String,
    pub path: alloc::string::String,
    #[serde(default)]
    pub params: alloc::collections::BTreeMap<alloc::string::String, alloc::string::String>,
    #[serde(default)]
    pub headers: alloc::vec::Vec<(alloc::string::String, alloc::string::String)>,
    #[serde(default)]
    pub principal_id: Option<alloc::string::String>,
    pub request_body_handle: u32,
    pub response_body_handle: u32,
}

impl InboundHttp {
    /// Reader for the request body the kernel pumps into the
    /// exchange. Reads from `request_body_handle`.
    pub fn request_body(&self) -> HttpResponseBodyReader {
        HttpResponseBodyReader {
            handle: StreamHandle(self.request_body_handle),
        }
    }

    /// Writer for the response body the guest emits.
    pub fn response_body(&self) -> HttpRequestBodyWriter {
        HttpRequestBodyWriter {
            handle: StreamHandle(self.response_body_handle),
        }
    }

    /// Fire the HTTP response head. Must be called exactly once,
    /// before the guest finishes.
    pub fn submit_response_head(
        &self,
        status: u16,
        headers: &[(&str, &str)],
    ) -> Result<(), StreamError> {
        let header_pairs: alloc::vec::Vec<[alloc::string::String; 2]> = headers
            .iter()
            .map(|(k, v)| {
                [
                    alloc::string::String::from(*k),
                    alloc::string::String::from(*v),
                ]
            })
            .collect();
        let head = serde_json::json!({
            "status": status,
            "headers": header_pairs,
        })
        .to_string();
        let rc = unsafe {
            host::host_http_stream_send_response_head(
                self.response_body_handle as i32,
                head.as_ptr() as i32,
                head.len() as i32,
            )
        };
        if rc == 0 {
            Ok(())
        } else if rc == -3 {
            Err(StreamError::InvalidHandle)
        } else {
            Err(StreamError::Other(alloc::format!(
                "host_http_stream_send_response_head rc {rc}"
            )))
        }
    }
}

/// Open a streaming outbound HTTP exchange. Returns the two
/// body-stream wrappers plus a closure the guest calls to
/// retrieve the response head after finalising the request.
pub fn streaming_call(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<
    (
        HttpRequestBodyWriter,
        HttpResponseBodyReader,
        impl FnOnce() -> Result<HttpResponseHead, StreamError>,
    ),
    StreamError,
> {
    // Serialize headers as JSON `[[k,v],...]`.
    let header_pairs: alloc::vec::Vec<[alloc::string::String; 2]> = headers
        .iter()
        .map(|(k, v)| {
            [
                alloc::string::String::from(*k),
                alloc::string::String::from(*v),
            ]
        })
        .collect();
    let headers_json = serde_json::to_string(&header_pairs)
        .map_err(|e| StreamError::Other(format!("header encode: {e}")))?;

    let mut req_handle_buf = [0u8; 4];
    let mut resp_handle_buf = [0u8; 4];
    let rc = unsafe {
        host::host_http_stream_begin_outbound(
            method.as_ptr() as i32,
            method.len() as i32,
            url.as_ptr() as i32,
            url.len() as i32,
            headers_json.as_ptr() as i32,
            headers_json.len() as i32,
            req_handle_buf.as_mut_ptr() as i32,
            resp_handle_buf.as_mut_ptr() as i32,
        )
    };
    if rc != 0 {
        return Err(StreamError::Other(format!("begin_outbound rc {rc}")));
    }

    let req_handle = StreamHandle(u32::from_le_bytes(req_handle_buf));
    let resp_handle = StreamHandle(u32::from_le_bytes(resp_handle_buf));

    let head_fetcher = move || -> Result<HttpResponseHead, StreamError> {
        let mut buf = alloc::vec![0u8; 16 * 1024];
        let rc = unsafe {
            host::host_http_stream_response_head(
                resp_handle.0 as i32,
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
            )
        };
        if rc < 0 {
            return Err(StreamError::Other(format!("response_head rc {rc}")));
        }
        buf.truncate(rc as usize);
        #[derive(serde::Deserialize)]
        struct RawHead {
            status: u16,
            headers: alloc::vec::Vec<(alloc::string::String, alloc::string::String)>,
        }
        let parsed: RawHead = serde_json::from_slice(&buf)
            .map_err(|e| StreamError::Other(format!("head decode: {e}")))?;
        Ok(HttpResponseHead {
            status: parsed.status,
            headers: parsed.headers,
        })
    };

    Ok((
        HttpRequestBodyWriter { handle: req_handle },
        HttpResponseBodyReader { handle: resp_handle },
        head_fetcher,
    ))
}
