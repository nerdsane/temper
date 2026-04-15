//! OpenClaw gateway WebSocket adapter.

use std::time::Instant;

use async_trait::async_trait;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use temper_runtime::scheduler::sim_uuid;
use tokio_tungstenite::tungstenite::Message;

use super::{AdapterContext, AdapterError, AdapterResult, AgentAdapter};

/// Adapter implementation for OpenClaw gateway execution over WebSocket.
#[derive(Debug, Default)]
pub struct OpenClawAdapter;

#[async_trait]
impl AgentAdapter for OpenClawAdapter {
    fn adapter_type(&self) -> &str {
        "openclaw"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let started = Instant::now();

        let gateway_url = ctx
            .integration_config
            .get("gateway_url")
            .cloned()
            .unwrap_or_else(|| "ws://127.0.0.1:18789".to_string());

        // determinism-ok: WebSocket for agent gateway
        let (mut socket, _) = tokio_tungstenite::connect_async(&gateway_url)
            .await
            .map_err(|e| AdapterError::Invocation(format!("openclaw connect failed: {e}")))?;

        maybe_handle_challenge(&ctx, &mut socket).await?;

        let session_key = ctx
            .integration_config
            .get("session_key")
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "temper:agent:{}",
                    ctx.agent_ctx
                        .agent_id
                        .clone()
                        .unwrap_or_else(|| ctx.entity_id.clone())
                )
            });

        let prompt = ctx
            .integration_config
            .get("message")
            .cloned()
            .unwrap_or_else(|| {
                if ctx.trigger_params.is_null() {
                    ctx.trigger_action.clone()
                } else {
                    format!("{} {}", ctx.trigger_action, ctx.trigger_params)
                }
            });

        let request_id = sim_uuid().to_string();
        let request = serde_json::json!({
            "type": "req agent",
            "id": request_id,
            "sessionKey": session_key,
            "idempotencyKey": format!("temper:{}", sim_uuid()),
            "message": prompt,
            "metadata": {
                "tenant": ctx.tenant,
                "entity_type": ctx.entity_type,
                "entity_id": ctx.entity_id,
                "trigger_action": ctx.trigger_action,
            }
        });

        socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|e| AdapterError::Execution(format!("openclaw send failed: {e}")))?;

        let mut last_payload = serde_json::json!({});
        let mut terminal_seen = false;

        for _ in 0..512 {
            let next_frame =
                tokio::time::timeout(std::time::Duration::from_secs(30), socket.next())
                    .await
                    .map_err(|_| {
                        AdapterError::Execution(
                            "openclaw timed out waiting for response".to_string(),
                        )
                    })?;

            let Some(frame) = next_frame else {
                break;
            };

            let frame = frame
                .map_err(|e| AdapterError::Execution(format!("openclaw receive failed: {e}")))?;

            if let Some(text) = frame_to_text(frame)
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
            {
                if is_terminal_frame(&json) {
                    last_payload = json;
                    terminal_seen = true;
                    break;
                }
                last_payload = json;
            }
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        if terminal_seen {
            Ok(AdapterResult::success(last_payload, duration_ms))
        } else {
            Ok(AdapterResult::failure(
                "openclaw execution ended without terminal frame".to_string(),
                duration_ms,
            ))
        }
    }
}

async fn maybe_handle_challenge(
    ctx: &AdapterContext,
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<(), AdapterError> {
    let Some(device_key_hex) = ctx
        .integration_config
        .get("device_key")
        .cloned()
        .or_else(|| ctx.get_secret("openclaw_device_key"))
    else {
        return Ok(());
    };

    let first_frame = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await
        .ok()
        .flatten()
        .transpose()
        .map_err(|e| AdapterError::Execution(format!("openclaw challenge read failed: {e}")))?;

    let Some(first_frame) = first_frame else {
        return Ok(());
    };

    let Some(text) = frame_to_text(first_frame) else {
        return Ok(());
    };

    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(());
    };

    let frame_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if frame_type != "connect.challenge" {
        return Ok(());
    }

    let nonce = json
        .get("nonce")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .as_bytes()
        .to_vec();

    let key_bytes = decode_hex_32(&device_key_hex).ok_or_else(|| {
        AdapterError::Invocation("invalid openclaw device key (expected 32-byte hex)".to_string())
    })?;
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let signature = signing_key.sign(&nonce).to_bytes();

    let auth = serde_json::json!({
        "type": "connect.auth",
        "signature": encode_hex(&signature),
    });

    socket
        .send(Message::Text(auth.to_string().into()))
        .await
        .map_err(|e| AdapterError::Execution(format!("openclaw auth send failed: {e}")))?;

    Ok(())
}

fn frame_to_text(frame: Message) -> Option<String> {
    match frame {
        Message::Text(text) => Some(text.to_string()),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec()).ok(),
        Message::Close(_) => Some(String::new()),
        _ => None,
    }
}

fn is_terminal_frame(payload: &serde_json::Value) -> bool {
    let frame_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    matches!(
        frame_type,
        "agent.completed" | "agent.done" | "agent.error" | "agent.failed"
    ) || matches!(status, "Done" | "COMPLETED" | "Failed" | "ERROR")
}

fn decode_hex_32(input: &str) -> Option<[u8; 32]> {
    let value = input.trim();
    if value.len() != 64 {
        return None;
    }

    let mut bytes = [0u8; 32];
    for (i, slot) in bytes.iter_mut().enumerate() {
        let idx = i * 2;
        let byte = u8::from_str_radix(&value[idx..idx + 2], 16).ok()?;
        *slot = byte;
    }
    Some(bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}
