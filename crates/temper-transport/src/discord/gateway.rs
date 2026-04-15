//! Discord Gateway WebSocket lifecycle — connect, heartbeat, identify, resume.
//!
//! Pure platform I/O. No Temper business logic here.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use futures_util::SinkExt;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use super::types::*;

/// Discord REST API v10 base URL.
pub(crate) const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Discord Gateway connection state.
pub(crate) struct GatewayState {
    /// Bot's own user ID (populated after READY).
    pub bot_user_id: Arc<RwLock<String>>,
    /// Last sequence number received.
    pub sequence: Arc<AtomicU64>,
    /// Session ID for resume (populated after READY).
    pub session_id: Arc<RwLock<Option<String>>>,
    /// Resume gateway URL (populated after READY).
    pub resume_url: Arc<RwLock<Option<String>>>,
}

impl GatewayState {
    pub fn new() -> Self {
        Self {
            bot_user_id: Arc::new(RwLock::new(String::new())),
            sequence: Arc::new(AtomicU64::new(0)),
            session_id: Arc::new(RwLock::new(None)),
            resume_url: Arc::new(RwLock::new(None)),
        }
    }
}

/// Fetch the Gateway bot URL from Discord REST API.
pub(crate) async fn fetch_gateway_url(
    http: &reqwest::Client,
    bot_token: &str,
) -> Result<String, String> {
    let resp = http
        .get(format!("{DISCORD_API_BASE}/gateway/bot"))
        .header("Authorization", format!("Bot {bot_token}"))
        .send()
        .await
        .map_err(|e| format!("Failed to fetch gateway URL: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Gateway bot endpoint returned {status}: {body}"));
    }

    let bot_resp: GatewayBotResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse gateway response: {e}"))?;

    Ok(bot_resp.url)
}

/// Type alias for the WebSocket write half.
pub(crate) type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// Type alias for the WebSocket read half.
pub(crate) type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

/// Send Identify payload with presence.
pub(crate) async fn send_identify(
    write: &mut WsSink,
    bot_token: &str,
    intents: u32,
) -> Result<(), String> {
    let identify = IdentifyPayload {
        op: GatewayOpcode::Identify as u8,
        d: IdentifyData {
            token: bot_token.to_string(),
            intents,
            properties: ConnectionProperties {
                os: "linux".to_string(),
                browser: "temper".to_string(),
                device: "temper".to_string(),
            },
            presence: Some(PresenceUpdateData {
                since: None,
                activities: vec![],
                status: "online".to_string(),
                afk: false,
            }),
        },
    };
    let json = serde_json::to_string(&identify)
        .map_err(|e| format!("Failed to serialize Identify: {e}"))?;
    write
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| format!("Identify send failed: {e}"))?;
    Ok(())
}

/// Send Resume payload.
pub(crate) async fn send_resume(
    write: &mut WsSink,
    bot_token: &str,
    session_id: &str,
    sequence: u64,
) -> Result<(), String> {
    let resume = ResumePayload {
        op: GatewayOpcode::Resume as u8,
        d: ResumeData {
            token: bot_token.to_string(),
            session_id: session_id.to_string(),
            seq: sequence,
        },
    };
    let json =
        serde_json::to_string(&resume).map_err(|e| format!("Failed to serialize Resume: {e}"))?;
    write
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| format!("Resume send failed: {e}"))?;
    Ok(())
}

/// Send presence update (opcode 3).
pub(crate) async fn send_presence_online(write: &mut WsSink) -> Result<(), String> {
    let presence = serde_json::json!({
        "op": 3,
        "d": { "since": null, "activities": [], "status": "online", "afk": false }
    });
    let json = serde_json::to_string(&presence).unwrap_or_default();
    write
        .send(Message::Text(json.into()))
        .await
        .map_err(|e| format!("Presence send failed: {e}"))?;
    Ok(())
}

/// Parse a WebSocket frame into a Gateway payload.
pub(crate) fn parse_frame(frame: Message) -> Result<Option<GatewayPayload>, String> {
    let text = match frame {
        Message::Text(t) => t.to_string(),
        Message::Binary(b) => {
            String::from_utf8(b.to_vec()).map_err(|e| format!("Invalid UTF-8: {e}"))?
        }
        Message::Close(_) => return Ok(None),
        _ => return Ok(None),
    };
    let payload: GatewayPayload =
        serde_json::from_str(&text).map_err(|e| format!("Failed to parse payload: {e}"))?;
    Ok(Some(payload))
}

/// Handle READY event: extract bot user ID and session info.
pub(crate) async fn handle_ready(
    state: &GatewayState,
    data: serde_json::Value,
) -> Result<(), String> {
    let ready: ReadyData =
        serde_json::from_value(data).map_err(|e| format!("Failed to parse READY: {e}"))?;

    println!(
        "  [discord] Connected as {}#{} ({})",
        ready.user.username,
        ready.user.discriminator.as_deref().unwrap_or("0"),
        ready.user.id
    );

    *state.bot_user_id.write().await = ready.user.id;
    *state.session_id.write().await = Some(ready.session_id);
    *state.resume_url.write().await = Some(ready.resume_gateway_url);

    Ok(())
}

/// Send a message to a Discord channel via REST API.
pub async fn send_discord_message(
    http: &reqwest::Client,
    bot_token: &str,
    channel_id: &str,
    content: &str,
) -> Result<(), String> {
    let chunks = split_message(content, 2000);
    for chunk in chunks {
        let body = CreateMessageRequest {
            content: chunk.to_string(),
        };

        let resp = http
            .post(format!("{DISCORD_API_BASE}/channels/{channel_id}/messages"))
            .header("Authorization", format!("Bot {bot_token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Discord API error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord API returned {status}: {body}"));
        }
    }
    Ok(())
}

/// Send typing indicator.
pub(crate) async fn send_typing(http: &reqwest::Client, bot_token: &str, channel_id: &str) {
    let _ = http
        .post(format!("{DISCORD_API_BASE}/channels/{channel_id}/typing"))
        .header("Authorization", format!("Bot {bot_token}"))
        .send()
        .await;
}

/// UTF-8 safe truncation.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

/// Split a message into chunks that fit within Discord's character limit.
/// UTF-8 safe — uses floor_char_boundary to avoid splitting multi-byte chars.
fn split_message(content: &str, max_len: usize) -> Vec<&str> {
    if content.len() <= max_len {
        return vec![content];
    }

    let mut chunks = Vec::new();
    let mut remaining = content;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining);
            break;
        }

        let boundary = remaining.floor_char_boundary(max_len);
        let split_at = remaining[..boundary].rfind('\n').unwrap_or(boundary);

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk);
        remaining = rest.trim_start_matches('\n');
    }

    chunks
}

/// Log a truncated message from a user.
pub(crate) fn log_message(username: &str, content: &str) {
    println!(
        "  [discord] Message from {username}: {}",
        truncate(content, 80)
    );
}
