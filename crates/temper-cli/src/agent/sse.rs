//! Generic SSE (Server-Sent Events) client.
//!
//! Parses `event:`, `data:`, `id:` fields per the SSE specification.
//! Supports reconnection with `Last-Event-ID` header.
//! Returns `impl Stream<Item = Result<SseEvent>>`.

use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::Result;
use futures_core::Stream;
use reqwest::Client;

/// A single SSE event parsed from the stream.
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// The event type (from `event:` field). `None` means "message".
    pub event_type: Option<String>,
    /// The event data (from `data:` field(s), joined with newlines).
    pub data: String,
    /// The event ID (from `id:` field).
    #[allow(dead_code)]
    pub id: Option<String>,
}

/// SSE client that connects to an SSE endpoint and yields events.
pub struct SseClient {
    url: String,
    client: Client,
}

impl SseClient {
    /// Create a new SSE client for the given URL.
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: Client::new(),
        }
    }

    /// Create a new SSE client with a shared reqwest client.
    #[allow(dead_code)]
    pub fn with_client(url: &str, client: Client) -> Self {
        Self {
            url: url.to_string(),
            client,
        }
    }

    /// Subscribe to the SSE endpoint and return a stream of events.
    pub async fn subscribe(&self) -> Result<SseStream> {
        self.subscribe_from(None).await
    }

    /// Subscribe to the SSE endpoint, optionally resuming from a last event ID.
    pub async fn subscribe_from(&self, last_event_id: Option<&str>) -> Result<SseStream> {
        let mut request = self
            .client
            .get(&self.url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache");

        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("SSE connection failed ({status}): {body}");
        }

        let byte_stream = response.bytes_stream();
        Ok(SseStream {
            inner: Box::pin(byte_stream),
            buffer: String::new(),
            current_event_type: None,
            current_data: Vec::new(),
            current_id: None,
        })
    }
}

/// A stream of SSE events parsed from a byte stream.
pub struct SseStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buffer: String,
    current_event_type: Option<String>,
    current_data: Vec<String>,
    current_id: Option<String>,
}

impl SseStream {
    /// Create an `SseStream` from a raw byte stream (e.g. from `response.bytes_stream()`).
    pub fn from_byte_stream(
        stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(stream),
            buffer: String::new(),
            current_event_type: None,
            current_data: Vec::new(),
            current_id: None,
        }
    }

    /// Try to parse buffered lines into SSE events.
    /// Returns `Some(event)` if a complete event was found (blank line delimiter).
    fn try_parse_event(&mut self) -> Option<SseEvent> {
        loop {
            // Find the next line boundary.
            let newline_pos = self.buffer.find('\n')?;
            let line = self.buffer[..newline_pos].trim_end_matches('\r').to_string();
            self.buffer = self.buffer[newline_pos + 1..].to_string();

            if line.is_empty() {
                // Blank line = event dispatch.
                if self.current_data.is_empty() {
                    // No data accumulated — reset and continue.
                    self.current_event_type = None;
                    self.current_id = None;
                    continue;
                }

                let event = SseEvent {
                    event_type: self.current_event_type.take(),
                    data: self.current_data.join("\n"),
                    id: self.current_id.take(),
                };
                self.current_data.clear();
                return Some(event);
            }

            // Parse field.
            if let Some(value) = line.strip_prefix("data:") {
                self.current_data.push(value.trim_start().to_string());
            } else if let Some(value) = line.strip_prefix("event:") {
                self.current_event_type = Some(value.trim_start().to_string());
            } else if let Some(value) = line.strip_prefix("id:") {
                self.current_id = Some(value.trim_start().to_string());
            }
            // Ignore `retry:` and comment lines (starting with `:`)
        }
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // First, try to yield an event from buffered data.
        if let Some(event) = self.try_parse_event() {
            return Poll::Ready(Some(Ok(event)));
        }

        // Read more bytes from the underlying stream.
        loop {
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    let text = String::from_utf8_lossy(&bytes);
                    self.buffer.push_str(&text);

                    // Try to parse an event from the new data.
                    if let Some(event) = self.try_parse_event() {
                        return Poll::Ready(Some(Ok(event)));
                    }
                    // No complete event yet — continue reading.
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e.into())));
                }
                Poll::Ready(None) => {
                    // Stream ended. Flush any remaining event.
                    if !self.current_data.is_empty() {
                        let event = SseEvent {
                            event_type: self.current_event_type.take(),
                            data: self.current_data.join("\n"),
                            id: self.current_id.take(),
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
