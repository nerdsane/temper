//! SSE (Server-Sent Events) stream parsing utilities.

use anyhow::{Context, Result};
use futures_util::stream::{Stream, StreamExt};

use crate::types::EntityEvent;

/// Parse an SSE byte stream into a stream of [`EntityEvent`]s.
///
/// Follows the SSE protocol: lines starting with `data:` contain JSON payloads,
/// blank lines delimit events. Lines starting with `:` are comments and ignored.
pub fn parse_sse_stream(
    byte_stream: impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin + Send + 'static,
) -> impl Stream<Item = Result<EntityEvent>> {
    futures_util::stream::unfold(
        (byte_stream, String::new()),
        |(mut stream, mut buffer)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.push_str(&String::from_utf8_lossy(&chunk));

                        // Process complete lines.
                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                            buffer = buffer[newline_pos + 1..].to_string();

                            // Skip comments and empty lines.
                            if line.starts_with(':') || line.is_empty() {
                                continue;
                            }

                            if let Some(data) = line.strip_prefix("data:") {
                                let data = data.trim();
                                if data.is_empty() {
                                    continue;
                                }
                                match serde_json::from_str::<EntityEvent>(data)
                                    .context("Failed to parse SSE event data")
                                {
                                    Ok(event) => return Some((Ok(event), (stream, buffer))),
                                    Err(e) => return Some((Err(e), (stream, buffer))),
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Some((
                            Err(anyhow::anyhow!("SSE stream error: {e}")),
                            (stream, buffer),
                        ));
                    }
                    None => return None, // Stream ended.
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use serde_json::json;

    #[tokio::test]
    async fn test_parse_sse_data_line() {
        let event_json = json!({
            "entity_type": "Tasks",
            "entity_id": "t-1",
            "action": "Start",
            "data": {}
        });
        let raw = format!("data: {}\n\n", event_json);
        let byte_stream = stream::iter(vec![Ok(bytes::Bytes::from(raw))]);

        let mut events = Box::pin(parse_sse_stream(byte_stream));
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.entity_type, "Tasks");
        assert_eq!(event.entity_id, "t-1");
        assert_eq!(event.action, "Start");
    }

    #[tokio::test]
    async fn test_parse_sse_ignores_comments() {
        let event_json = json!({
            "entity_type": "Agents",
            "entity_id": "a-1",
            "action": "Assign",
            "data": {"role": "tester"}
        });
        let raw = format!(": keep-alive\ndata: {}\n\n", event_json);
        let byte_stream = stream::iter(vec![Ok(bytes::Bytes::from(raw))]);

        let mut events = Box::pin(parse_sse_stream(byte_stream));
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.entity_type, "Agents");
    }

    #[tokio::test]
    async fn test_parse_sse_empty_stream() {
        let byte_stream = stream::iter(Vec::<reqwest::Result<bytes::Bytes>>::new());
        let mut events = Box::pin(parse_sse_stream(byte_stream));
        assert!(events.next().await.is_none());
    }
}
