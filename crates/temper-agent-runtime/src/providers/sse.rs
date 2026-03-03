//! Shared SSE (Server-Sent Events) parser for streaming LLM responses.
//!
//! Both [`super::anthropic::AnthropicProvider`] and [`super::codex::CodexProvider`]
//! use SSE streaming but parse different event content.

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use futures_core::Stream;

/// A parsed SSE event from a streaming LLM API.
#[derive(Debug)]
pub(crate) struct SseEvent {
    /// The event type (from `event:` lines).
    pub event_type: Option<String>,
    /// The event data (from `data:` lines, joined with newlines).
    pub data: String,
}

/// Minimal SSE parser over a byte stream.
pub(crate) struct SseByteStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
    current_event_type: Option<String>,
    current_data: Vec<String>,
}

impl SseByteStream {
    /// Create a new SSE parser wrapping a byte stream (e.g. from `reqwest::Response::bytes_stream()`).
    pub fn new(stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
            buffer: String::new(),
            current_event_type: None,
            current_data: Vec::new(),
        }
    }

    fn try_parse_event(&mut self) -> Option<SseEvent> {
        loop {
            let newline_pos = self.buffer.find('\n')?;
            let line = self.buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                if self.current_data.is_empty() {
                    self.current_event_type = None;
                    continue;
                }
                let event = SseEvent {
                    event_type: self.current_event_type.take(),
                    data: self.current_data.join("\n"),
                };
                self.current_data.clear();
                return Some(event);
            }

            if let Some(value) = line.strip_prefix("data:") {
                self.current_data.push(value.trim_start().to_string());
            } else if let Some(value) = line.strip_prefix("event:") {
                self.current_event_type = Some(value.trim_start().to_string());
            }
        }
    }
}

impl Stream for SseByteStream {
    type Item = Result<SseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.try_parse_event() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&text);

                    if let Some(event) = self.try_parse_event() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => {
                    if !self.current_data.is_empty() {
                        let event = SseEvent {
                            event_type: self.current_event_type.take(),
                            data: self.current_data.join("\n"),
                        };
                        self.current_data.clear();
                        return Poll::Ready(Some(Ok(event)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
