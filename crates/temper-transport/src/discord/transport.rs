//! Discord transport — wires Discord Gateway to Temper Channel entities.
//!
//! On startup: bootstraps Channel + default AgentRoute entities.
//! On MESSAGE_CREATE: dispatches Channel.ReceiveMessage via OData API.
//! On Channel.SendReply events: delivers reply via Discord REST API.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use super::gateway::*;
use super::types::*;
use crate::TemperApiClient;

/// Configuration for the Discord transport.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Discord bot token.
    pub bot_token: String,
    /// Gateway intents bitmask.
    pub intents: u32,
}

/// Discord channel transport.
///
/// Connects to Discord Gateway, dispatches messages to Temper Channel entities
/// via the OData API, and delivers replies via Discord REST API.
pub struct DiscordTransport {
    config: DiscordConfig,
    api: TemperApiClient,
    http: reqwest::Client,
    gateway: GatewayState,
    /// Channel entity ID in Temper (populated on startup).
    channel_entity_id: Arc<RwLock<Option<String>>>,
    /// Maps Discord channel_id (DM channel) → user_id for reply routing.
    dm_channels: Arc<RwLock<BTreeMap<String, String>>>,
}

impl DiscordTransport {
    /// Create a new Discord transport.
    pub fn new(config: DiscordConfig, api: TemperApiClient) -> Self {
        Self {
            config,
            api,
            http: reqwest::Client::new(),
            gateway: GatewayState::new(),
            channel_entity_id: Arc::new(RwLock::new(None)),
            dm_channels: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Run the transport indefinitely.
    pub async fn run(&self) -> Result<(), String> {
        // Phase 1: Bootstrap Channel + AgentRoute entities.
        self.bootstrap_channel().await?;

        // Phase 2: Connect to Discord Gateway.
        let gateway_url = fetch_gateway_url(&self.http, &self.config.bot_token).await?;
        println!("  [discord] Gateway URL: {gateway_url}");

        // Phase 3: Spawn reply watcher.
        self.spawn_reply_watcher();

        // Phase 4: Event loop with reconnection.
        let mut backoff = Duration::from_secs(1);
        let mut url = format!("{gateway_url}/?v=10&encoding=json");

        loop {
            match self.connect_and_run(&url).await {
                Ok(()) => backoff = Duration::from_secs(1),
                Err(e) => {
                    eprintln!("  [discord] Gateway error: {e}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }

            if let Some(resume) = self.gateway.resume_url.read().await.as_ref() {
                url = format!("{resume}/?v=10&encoding=json");
            }

            println!("  [discord] Reconnecting...");
        }
    }

    /// Bootstrap the Channel entity and default AgentRoute.
    async fn bootstrap_channel(&self) -> Result<(), String> {
        // Check for existing discord Channel entity.
        let existing = self
            .api
            .query_entities("Channels", "ChannelType eq 'discord'")
            .await
            .unwrap_or_default();

        let channel_id = if let Some(first) = existing.first() {
            let id = first
                .get("Id")
                .or_else(|| first.get("entity_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            println!("  [discord] Found existing Channel entity: {id}");
            id
        } else {
            // Create new Channel entity.
            let resp = self
                .api
                .create_entity(
                    "Channels",
                    serde_json::json!({
                        "ChannelType": "discord",
                    }),
                )
                .await?;
            let id = resp
                .get("entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            println!("  [discord] Created Channel entity: {id}");

            // Configure the channel.
            let _ = self
                .api
                .dispatch_action(
                    "Channels",
                    &id,
                    "Configure",
                    serde_json::json!({
                        "channel_type": "discord",
                        "channel_id": "discord-gateway",
                    }),
                )
                .await;

            // Connect → Ready.
            let _ = self
                .api
                .dispatch_action("Channels", &id, "Connect", serde_json::json!({}))
                .await;

            id
        };

        if channel_id.is_empty() {
            return Err("Failed to bootstrap Channel entity".to_string());
        }

        *self.channel_entity_id.write().await = Some(channel_id.clone());

        // Ensure at least one AgentRoute exists.
        let routes = self
            .api
            .query_entities("AgentRoutes", &format!("ChannelId eq '{channel_id}'"))
            .await
            .unwrap_or_default();

        if routes.is_empty() {
            let route_resp = self
                .api
                .create_entity("AgentRoutes", serde_json::json!({}))
                .await;

            if let Ok(route) = route_resp {
                let route_id = route
                    .get("entity_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let _ = self
                    .api
                    .dispatch_action(
                        "AgentRoutes",
                        route_id,
                        "Register",
                        serde_json::json!({
                            "binding_tier": "channel",
                            "channel_id": channel_id,
                            "agent_config": serde_json::json!({
                                "system_prompt": "You are a helpful AI assistant. Be concise and conversational.",
                                "model": "claude-sonnet-4-20250514",
                            }).to_string(),
                        }),
                    )
                    .await;
                println!("  [discord] Created default AgentRoute: {route_id}");
            }
        } else {
            println!("  [discord] Found {} existing AgentRoute(s)", routes.len());
        }

        Ok(())
    }

    /// Connect to Gateway and run the event loop.
    async fn connect_and_run(&self, url: &str) -> Result<(), String> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (mut write, mut read) = ws.split();

        // Wait for Hello (op 10).
        let hello = read_payload(&mut read)
            .await?
            .ok_or("Connection closed before Hello")?;

        if hello.op != GatewayOpcode::Hello as u8 {
            return Err(format!("Expected Hello (op 10), got op {}", hello.op));
        }

        let hello_data: HelloData =
            serde_json::from_value(hello.d.ok_or("Hello missing data field")?)
                .map_err(|e| format!("Failed to parse Hello: {e}"))?;

        let heartbeat_interval = Duration::from_millis(hello_data.heartbeat_interval);

        // Send Identify or Resume.
        let can_resume = self.gateway.session_id.read().await.is_some();
        if can_resume {
            let sid = self.gateway.session_id.read().await.clone().unwrap();
            let seq = self.gateway.sequence.load(Ordering::Relaxed);
            send_resume(&mut write, &self.config.bot_token, &sid, seq).await?;
        } else {
            send_identify(&mut write, &self.config.bot_token, self.config.intents).await?;
        }

        // Send presence.
        let _ = send_presence_online(&mut write).await;

        // Heartbeat ticker.
        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            loop {
                interval.tick().await;
                if heartbeat_tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        // Main event loop.
        loop {
            tokio::select! {
                frame = read.next() => {
                    let Some(frame) = frame else {
                        return Ok(());
                    };
                    let frame = frame.map_err(|e| format!("WebSocket read error: {e}"))?;
                    let Some(payload) = parse_frame(frame)? else {
                        continue;
                    };
                    let should_reconnect = self.handle_payload(payload).await?;
                    if should_reconnect {
                        return Ok(());
                    }
                }
                Some(()) = heartbeat_rx.recv() => {
                    let s = self.gateway.sequence.load(Ordering::Relaxed);
                    let payload = HeartbeatPayload {
                        op: GatewayOpcode::Heartbeat as u8,
                        d: if s > 0 { Some(s) } else { None },
                    };
                    let json = serde_json::to_string(&payload).unwrap_or_default();
                    write
                        .send(Message::Text(json.into()))
                        .await
                        .map_err(|e| format!("Heartbeat send failed: {e}"))?;
                }
            }
        }
    }

    /// Handle a Gateway payload.
    async fn handle_payload(&self, payload: GatewayPayload) -> Result<bool, String> {
        if let Some(s) = payload.s {
            self.gateway.sequence.store(s, Ordering::Relaxed);
        }

        match GatewayOpcode::from_u8(payload.op) {
            Some(GatewayOpcode::Dispatch) => {
                let event_name = payload.t.as_deref().unwrap_or("");
                match event_name {
                    "READY" => {
                        if let Some(d) = payload.d {
                            handle_ready(&self.gateway, d).await?;
                        }
                    }
                    "MESSAGE_CREATE" => {
                        if let Some(d) = payload.d {
                            self.handle_message_create(d).await;
                        }
                    }
                    _ => {}
                }
                Ok(false)
            }
            Some(GatewayOpcode::HeartbeatAck) => Ok(false),
            Some(GatewayOpcode::Reconnect) => {
                println!("  [discord] Server requested reconnect");
                Ok(true)
            }
            Some(GatewayOpcode::InvalidSession) => {
                let resumable = payload.d.and_then(|v| v.as_bool()).unwrap_or(false);
                if !resumable {
                    *self.gateway.session_id.write().await = None;
                }
                println!("  [discord] Invalid session (resumable={resumable})");
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Handle MESSAGE_CREATE: dispatch Channel.ReceiveMessage via OData API.
    ///
    /// All routing, agent creation, and session management is handled by
    /// the route_message WASM module triggered by Channel.ReceiveMessage.
    async fn handle_message_create(&self, data: serde_json::Value) {
        let msg: MessageCreateData = match serde_json::from_value(data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  [discord] Failed to parse MESSAGE_CREATE: {e}");
                return;
            }
        };

        // Ignore bot's own messages.
        let bot_id = self.gateway.bot_user_id.read().await.clone();
        if msg.author.id == bot_id || msg.author.bot {
            return;
        }

        // DMs only for now.
        if msg.guild_id.is_some() {
            return;
        }

        log_message(&msg.author.username, &msg.content);

        // Track DM channel → user mapping for reply delivery.
        self.dm_channels
            .write()
            .await
            .insert(msg.author.id.clone(), msg.channel_id.clone());

        // Send typing indicator.
        send_typing(&self.http, &self.config.bot_token, &msg.channel_id).await;

        // Dispatch Channel.ReceiveMessage — the WASM handles everything else.
        let channel_entity_id = self.channel_entity_id.read().await.clone();
        let Some(channel_id) = channel_entity_id else {
            eprintln!("  [discord] No Channel entity bootstrapped");
            return;
        };

        let params = serde_json::json!({
            "message_id": msg.id,
            "author_id": msg.author.id,
            "thread_id": msg.author.id,  // DMs use author_id as thread
            "content": msg.content,
        });

        match self
            .api
            .dispatch_action("Channels", &channel_id, "ReceiveMessage", params)
            .await
        {
            Ok(_) => {
                println!(
                    "  [discord] Dispatched ReceiveMessage for {}",
                    msg.author.username
                );
            }
            Err(e) => {
                eprintln!("  [discord] ReceiveMessage failed: {e}");
                // Send error message to user.
                let _ = send_discord_message(
                    &self.http,
                    &self.config.bot_token,
                    &msg.channel_id,
                    "Sorry, I encountered an error processing your message.",
                )
                .await;
            }
        }
    }

    /// Spawn a task that watches for Channel.SendReply events and delivers replies.
    fn spawn_reply_watcher(&self) {
        let api = self.api.clone();
        let http = self.http.clone();
        let bot_token = self.config.bot_token.clone();
        let dm_channels = self.dm_channels.clone();
        let channel_entity_id = self.channel_entity_id.clone();

        tokio::spawn(async move {
            // Poll for SendReply events on the Channel entity.
            // In the future this can use SSE/WebSocket subscription.
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;

                let Some(ref _channel_id) = *channel_entity_id.read().await else {
                    continue;
                };

                // For now, watch for TemperAgent completion events via the
                // observe API. The route_message WASM dispatches SendReply
                // which triggers send_reply WASM. The transport watches for
                // the reply content on the Channel entity.
                //
                // This is a placeholder — the proper implementation uses
                // event_tx subscription or SSE from the observe endpoint.
                // For the initial migration, we'll use the same pattern as
                // the current transport but via OData API calls.
                let _ = (&api, &http, &bot_token, &dm_channels);
            }
        });
    }
}

/// Read one Gateway payload from the WebSocket with timeout.
async fn read_payload(read: &mut WsStream) -> Result<Option<GatewayPayload>, String> {
    let frame = tokio::time::timeout(Duration::from_secs(60), read.next())
        .await
        .map_err(|_| "Timed out waiting for Gateway payload".to_string())?;
    let Some(frame) = frame else {
        return Ok(None);
    };
    let frame = frame.map_err(|e| format!("WebSocket read error: {e}"))?;
    parse_frame(frame)
}
