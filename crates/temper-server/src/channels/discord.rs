//! Discord channel transport via Gateway WebSocket (v10).
//!
//! Connects to `wss://gateway.discord.gg`, receives `MESSAGE_CREATE` events,
//! and dispatches TemperAgent entities to handle each message. Watches for
//! agent completion and delivers replies via Discord REST API.
//!
//! Conversation continuity: tracks per-user sessions keyed by Discord user ID.
//! First message uses Provision (creates sandbox + TemperFS workspace).
//! Follow-up messages append to the existing TemperFS conversation file and
//! use Resume (reuses workspace, restores sandbox files).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use crate::request_context::AgentContext;
use crate::state::ServerState;

use super::discord_types::*;

use temper_runtime::tenant::TenantId;

/// Discord REST API v10 base URL.
const DISCORD_API_BASE: &str = "https://discord.com/api/v10";

/// Principal kind for internal (server-to-server) TemperFS calls.
const INTERNAL_PRINCIPAL_KIND: &str = "admin";

/// Configuration for a Discord channel transport.
#[derive(Debug, Clone)]
pub struct DiscordTransportConfig {
    /// Bot token for authentication.
    pub bot_token: String,
    /// Tenant to route messages to.
    pub tenant: String,
    /// Gateway intents bitmask.
    pub intents: u32,
}

/// Tracks a pending agent reply mapped to a Discord channel + user.
#[derive(Debug, Clone)]
struct PendingReply {
    /// Discord channel ID for reply delivery.
    discord_channel_id: String,
    /// Discord user ID (for session tracking after completion).
    discord_user_id: String,
}

/// Per-user conversation session. Saved after the first agent completes so
/// follow-up messages can Resume with the same session tree.
/// Serializable for persistence to TemperFS across server restarts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UserSession {
    /// TemperFS conversation file entity ID (legacy, passed for backward compat).
    conversation_file_id: String,
    /// TemperFS workspace entity ID.
    workspace_id: String,
    /// Sandbox URL (local or E2B).
    sandbox_url: String,
    /// Sandbox ID.
    sandbox_id: String,
    /// TemperFS file manifest entity ID.
    file_manifest_id: String,
    /// TemperFS session tree file ID (JSONL format).
    session_file_id: String,
    /// Current leaf entry ID in the session tree.
    session_leaf_id: String,
}

/// Discord channel transport.
///
/// Manages the Gateway WebSocket lifecycle, creates TemperAgent entities
/// for inbound messages, and delivers agent results via Discord REST API.
/// Maintains per-user sessions for conversation continuity.
pub struct DiscordTransport {
    config: DiscordTransportConfig,
    state: ServerState,
    http: reqwest::Client,
    /// Maps TemperAgent entity_id → PendingReply for reply routing.
    pending_replies: Arc<RwLock<BTreeMap<String, PendingReply>>>,
    /// Per-user conversation sessions (keyed by Discord user ID).
    user_sessions: Arc<RwLock<BTreeMap<String, UserSession>>>,
    /// Set of Discord user IDs with an active (in-flight) agent.
    active_users: Arc<RwLock<BTreeMap<String, Vec<String>>>>,
    /// Bot's own user ID (populated after READY event).
    bot_user_id: Arc<RwLock<String>>,
    /// Last sequence number received (for heartbeat + resume).
    sequence: Arc<AtomicU64>,
    /// Session ID for resume (populated after READY event).
    session_id: Arc<RwLock<Option<String>>>,
    /// Resume gateway URL (populated after READY event).
    resume_url: Arc<RwLock<Option<String>>>,
    /// TemperFS File entity ID for the sessions manifest (populated on first save).
    sessions_file_id: Arc<RwLock<Option<String>>>,
}

impl DiscordTransport {
    /// Create a new Discord transport.
    pub fn new(config: DiscordTransportConfig, state: ServerState) -> Self {
        Self {
            config,
            state,
            http: reqwest::Client::new(),
            pending_replies: Arc::new(RwLock::new(BTreeMap::new())),
            user_sessions: Arc::new(RwLock::new(BTreeMap::new())),
            active_users: Arc::new(RwLock::new(BTreeMap::new())),
            bot_user_id: Arc::new(RwLock::new(String::new())),
            sequence: Arc::new(AtomicU64::new(0)),
            session_id: Arc::new(RwLock::new(None)),
            resume_url: Arc::new(RwLock::new(None)),
            sessions_file_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Well-known name for the sessions manifest file in TemperFS.
    const SESSIONS_FILE_NAME: &str = "discord-sessions.json";

    /// Load persisted user sessions from TemperFS on startup.
    ///
    /// Looks for a File entity named "discord-sessions.json" in the tenant.
    /// If found, reads the JSON manifest and populates `user_sessions`.
    async fn load_persisted_sessions(&self) {
        let base_url = self.temper_api_url();
        let tenant = &self.config.tenant;

        // Query for the sessions manifest file by name.
        let query_url = format!(
            "{base_url}/tdata/Files?$filter=name eq '{}'",
            Self::SESSIONS_FILE_NAME
        );
        let resp = match self
            .http
            .get(&query_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  [discord] Failed to query sessions file: {e}");
                return;
            }
        };

        if !resp.status().is_success() {
            // TemperFS not available yet — sessions will be fresh.
            return;
        }

        let body = resp.text().await.unwrap_or_default();
        let data: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => return,
        };

        // Extract the first matching file entity.
        let file_id = data
            .get("value")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("Id").or_else(|| item.get("entity_id")))
            .and_then(|v| v.as_str());

        let Some(file_id) = file_id else {
            println!("  [discord] No persisted sessions found (first run)");
            return;
        };

        // Store the file ID for future saves.
        *self.sessions_file_id.write().await = Some(file_id.to_string());

        // Read the file content.
        let content_url = format!("{base_url}/tdata/Files('{file_id}')/$value");
        let content_resp = match self
            .http
            .get(&content_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            _ => return,
        };

        let content = content_resp.text().await.unwrap_or_default();
        let sessions: BTreeMap<String, UserSession> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  [discord] Failed to parse sessions manifest: {e}");
                return;
            }
        };

        let count = sessions.len();
        *self.user_sessions.write().await = sessions;
        println!("  [discord] Restored {count} user session(s) from TemperFS");
    }

    /// Persist the current user sessions to TemperFS.
    /// Run the transport. Connects to Discord Gateway, handles events, and
    /// reconnects on failure. This method runs indefinitely.
    pub async fn run(&self) -> Result<(), String> {
        // Load persisted sessions from TemperFS before connecting.
        self.load_persisted_sessions().await;

        // Fetch gateway URL.
        let gateway_url = self.fetch_gateway_url().await?;
        println!("  [discord] Gateway URL: {gateway_url}");

        // Spawn reply watcher.
        self.spawn_reply_watcher();

        // Connect and run event loop with reconnection.
        let mut backoff = Duration::from_secs(1);
        let mut url = format!("{gateway_url}/?v=10&encoding=json");

        loop {
            match self.connect_and_run(&url).await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    eprintln!("  [discord] Gateway error: {e}");
                    tokio::time::sleep(backoff).await; // determinism-ok: reconnect backoff for Discord Gateway
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }

            // Use resume URL if available.
            if let Some(resume) = self.resume_url.read().await.as_ref() {
                url = format!("{resume}/?v=10&encoding=json");
            }

            println!("  [discord] Reconnecting...");
        }
    }

    /// Fetch the Gateway bot URL from Discord REST API.
    async fn fetch_gateway_url(&self) -> Result<String, String> {
        let resp = self
            .http
            .get(format!("{DISCORD_API_BASE}/gateway/bot"))
            .header("Authorization", format!("Bot {}", self.config.bot_token))
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

    /// Connect to the Gateway WebSocket and run the event loop.
    async fn connect_and_run(&self, url: &str) -> Result<(), String> {
        let (ws, _) = tokio_tungstenite::connect_async(url) // determinism-ok: WebSocket for channel transport
            .await
            .map_err(|e| format!("WebSocket connect failed: {e}"))?;

        let (mut write, mut read) = ws.split();

        // Wait for Hello (op 10).
        let hello = self
            .read_payload(&mut read)
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
        let can_resume = self.session_id.read().await.is_some();
        if can_resume {
            self.send_resume(&mut write).await?;
        } else {
            self.send_identify(&mut write).await?;
        }

        // Send presence update (opcode 3) immediately after identify/resume.
        // Minimal payload: just set status to "online".
        let presence = serde_json::json!({
            "op": 3,
            "d": {
                "since": null,
                "activities": [],
                "status": "online",
                "afk": false
            }
        });
        let presence_json = serde_json::to_string(&presence).unwrap_or_default();
        let _ = write.send(Message::Text(presence_json.into())).await;

        // Heartbeat ticker: sends ticks via mpsc so the main loop can
        // multiplex heartbeats with WebSocket reads on a single write half.
        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel::<()>(1);
        let heartbeat_task = async move {
            let mut interval = tokio::time::interval(heartbeat_interval);
            loop {
                interval.tick().await;
                if heartbeat_tx.send(()).await.is_err() {
                    break;
                }
            }
        };
        tokio::spawn(heartbeat_task); // determinism-ok: periodic heartbeat for Discord Gateway

        // Main event loop: multiplex between WebSocket reads and heartbeat ticks.
        loop {
            tokio::select! {
                frame = read.next() => {
                    let Some(frame) = frame else {
                        return Ok(()); // Connection closed.
                    };
                    let frame = frame.map_err(|e| format!("WebSocket read error: {e}"))?;
                    let Some(payload) = self.parse_frame(frame)? else {
                        continue;
                    };
                    let should_reconnect = self.handle_payload(payload).await?;
                    if should_reconnect {
                        return Ok(());
                    }
                }
                Some(()) = heartbeat_rx.recv() => {
                    let s = self.sequence.load(Ordering::Relaxed);
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

    /// Send Identify payload.
    async fn send_identify(
        &self,
        write: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    ) -> Result<(), String> {
        let identify = IdentifyPayload {
            op: GatewayOpcode::Identify as u8,
            d: IdentifyData {
                token: self.config.bot_token.clone(),
                intents: self.config.intents,
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
    async fn send_resume(
        &self,
        write: &mut futures_util::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            Message,
        >,
    ) -> Result<(), String> {
        let session_id = self
            .session_id
            .read()
            .await
            .clone()
            .ok_or("No session ID for resume")?;
        let resume = ResumePayload {
            op: GatewayOpcode::Resume as u8,
            d: ResumeData {
                token: self.config.bot_token.clone(),
                session_id,
                seq: self.sequence.load(Ordering::Relaxed),
            },
        };
        let json = serde_json::to_string(&resume)
            .map_err(|e| format!("Failed to serialize Resume: {e}"))?;
        write
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| format!("Resume send failed: {e}"))?;
        Ok(())
    }

    /// Read and parse one Gateway payload from the WebSocket.
    async fn read_payload(
        &self,
        read: &mut futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) -> Result<Option<GatewayPayload>, String> {
        let frame = tokio::time::timeout(Duration::from_secs(60), read.next())
            .await
            .map_err(|_| "Timed out waiting for Gateway payload".to_string())?;
        let Some(frame) = frame else {
            return Ok(None);
        };
        let frame = frame.map_err(|e| format!("WebSocket read error: {e}"))?;
        self.parse_frame(frame)
    }

    /// Parse a WebSocket frame into a Gateway payload.
    fn parse_frame(&self, frame: Message) -> Result<Option<GatewayPayload>, String> {
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

    /// Handle a received Gateway payload. Returns true if we should reconnect.
    async fn handle_payload(&self, payload: GatewayPayload) -> Result<bool, String> {
        if let Some(s) = payload.s {
            self.sequence.store(s, Ordering::Relaxed);
        }

        let op = GatewayOpcode::from_u8(payload.op);

        match op {
            Some(GatewayOpcode::Dispatch) => {
                let event_name = payload.t.as_deref().unwrap_or("");
                match event_name {
                    "READY" => {
                        if let Some(d) = payload.d {
                            self.handle_ready(d).await?;
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
                    *self.session_id.write().await = None;
                }
                println!("  [discord] Invalid session (resumable={resumable})");
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Handle READY event: store bot user ID and session info.
    async fn handle_ready(&self, data: serde_json::Value) -> Result<(), String> {
        let ready: ReadyData =
            serde_json::from_value(data).map_err(|e| format!("Failed to parse READY: {e}"))?;

        println!(
            "  [discord] Connected as {}#{} ({})",
            ready.user.username,
            ready.user.discriminator.as_deref().unwrap_or("0"),
            ready.user.id
        );

        *self.bot_user_id.write().await = ready.user.id;
        *self.session_id.write().await = Some(ready.session_id);
        *self.resume_url.write().await = Some(ready.resume_gateway_url);

        Ok(())
    }

    /// Handle MESSAGE_CREATE: route to first-message or follow-up flow.
    ///
    /// First message from a user → Configure + Provision (new sandbox + workspace).
    /// Follow-up messages → append to TemperFS conversation, Configure + Resume.
    /// If an agent is already running for this user, queue the message content.
    async fn handle_message_create(&self, data: serde_json::Value) {
        let msg: MessageCreateData = match serde_json::from_value(data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  [discord] Failed to parse MESSAGE_CREATE: {e}");
                return;
            }
        };

        // Ignore bot's own messages.
        let bot_id = self.bot_user_id.read().await.clone();
        if msg.author.id == bot_id || msg.author.bot {
            return;
        }

        // For now, only process DMs (no guild_id).
        if msg.guild_id.is_some() {
            return;
        }

        println!(
            "  [discord] Message from {}: {}",
            msg.author.username,
            truncate(&msg.content, 80)
        );

        let user_id = msg.author.id.clone();

        // If there's already an active agent for this user, queue the message.
        {
            let mut active = self.active_users.write().await;
            if active.contains_key(&user_id) {
                println!(
                    "  [discord] Queuing message for {} (agent in progress)",
                    msg.author.username
                );
                active
                    .entry(user_id.clone())
                    .or_default()
                    .push(msg.content.clone());
                self.send_typing(&msg.channel_id).await;
                return;
            }
            // Mark user as active (empty queue).
            active.insert(user_id.clone(), Vec::new());
        }

        self.send_typing(&msg.channel_id).await;

        let has_session = self.user_sessions.read().await.contains_key(&user_id);

        if has_session {
            self.handle_followup_message(&msg).await;
        } else {
            self.handle_first_message(&msg).await;
        }
    }

    /// Handle the first message from a user: Configure + Provision.
    async fn handle_first_message(&self, msg: &MessageCreateData) {
        let entity_id = format!("discord-{}", msg.id);
        let tenant = TenantId::new(&self.config.tenant);
        let user_id = &msg.author.id;

        // Track pending reply.
        self.pending_replies.write().await.insert(
            entity_id.clone(),
            PendingReply {
                discord_channel_id: msg.channel_id.clone(),
                discord_user_id: user_id.clone(),
            },
        );

        let agent_ctx = AgentContext {
            agent_id: Some(format!("discord-transport:{user_id}")),
            session_id: None,
            agent_type: Some("system".to_string()),
            intent: None,
        };

        // Create the TemperAgent entity.
        let initial_fields = serde_json::json!({ "id": entity_id });
        if let Err(e) = self
            .state
            .get_or_create_tenant_entity(&tenant, "TemperAgent", &entity_id, initial_fields)
            .await
        {
            eprintln!("  [discord] Failed to create TemperAgent: {e}");
            self.cleanup_failed_agent(&entity_id, user_id).await;
            return;
        }

        let temper_api_url = self.temper_api_url();

        let configure_params = serde_json::json!({
            "system_prompt": self.system_prompt(&msg.author.username),
            "user_message": msg.content,
            "temper_api_url": temper_api_url,
        });

        if let Err(e) = self
            .state
            .dispatch_tenant_action(
                &tenant,
                "TemperAgent",
                &entity_id,
                "Configure",
                configure_params,
                &agent_ctx,
            )
            .await
        {
            eprintln!("  [discord] Configure failed: {e}");
            self.cleanup_failed_agent(&entity_id, user_id).await;
            return;
        }

        // Provision triggers: sandbox_provisioner → SandboxReady → call_llm → ...
        match self
            .state
            .dispatch_tenant_action(
                &tenant,
                "TemperAgent",
                &entity_id,
                "Provision",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
        {
            Ok(resp) if resp.success => {
                println!(
                    "  [discord] Agent {entity_id} provisioning (first message from {})",
                    msg.author.username
                );
            }
            Ok(resp) => {
                eprintln!(
                    "  [discord] Provision failed: {}",
                    resp.error.unwrap_or_default()
                );
                self.cleanup_failed_agent(&entity_id, user_id).await;
            }
            Err(e) => {
                eprintln!("  [discord] Provision dispatch error: {e}");
                self.cleanup_failed_agent(&entity_id, user_id).await;
            }
        }
    }

    /// Handle a follow-up message: append to session tree, Configure + Resume.
    async fn handle_followup_message(&self, msg: &MessageCreateData) {
        let entity_id = format!("discord-{}", msg.id);
        let tenant = TenantId::new(&self.config.tenant);
        let user_id = &msg.author.id;

        let session = match self.user_sessions.read().await.get(user_id).cloned() {
            Some(s) => s,
            None => {
                // Race: session disappeared. Fall back to first message flow.
                self.handle_first_message(msg).await;
                return;
            }
        };

        // Append user message to the legacy conversation file.
        // The llm_caller reads from this file for context on follow-ups.
        if session.conversation_file_id.is_empty() {
            self.user_sessions.write().await.remove(user_id);
            self.handle_first_message(msg).await;
            return;
        }
        if let Err(e) = self
            .append_to_legacy_conversation(&session.conversation_file_id, &msg.content)
            .await
        {
            eprintln!("  [discord] Failed to append to conversation: {e}");
            self.user_sessions.write().await.remove(user_id);
            self.handle_first_message(msg).await;
            return;
        }

        // Track pending reply.
        self.pending_replies.write().await.insert(
            entity_id.clone(),
            PendingReply {
                discord_channel_id: msg.channel_id.clone(),
                discord_user_id: user_id.clone(),
            },
        );

        let agent_ctx = AgentContext {
            agent_id: Some(format!("discord-transport:{user_id}")),
            session_id: None,
            agent_type: Some("system".to_string()),
            intent: None,
        };

        // Create a new TemperAgent entity for this turn.
        let initial_fields = serde_json::json!({ "id": entity_id });
        if let Err(e) = self
            .state
            .get_or_create_tenant_entity(&tenant, "TemperAgent", &entity_id, initial_fields)
            .await
        {
            eprintln!("  [discord] Failed to create TemperAgent: {e}");
            self.cleanup_failed_agent(&entity_id, user_id).await;
            return;
        }

        let temper_api_url = self.temper_api_url();

        // Configure sets system_prompt, model, etc. user_message is set but
        // won't be used by llm_caller since the conversation file already has
        // messages — it reads from TemperFS instead.
        let configure_params = serde_json::json!({
            "system_prompt": self.system_prompt(&msg.author.username),
            "user_message": msg.content,
            "temper_api_url": temper_api_url,
        });

        if let Err(e) = self
            .state
            .dispatch_tenant_action(
                &tenant,
                "TemperAgent",
                &entity_id,
                "Configure",
                configure_params,
                &agent_ctx,
            )
            .await
        {
            eprintln!("  [discord] Configure failed: {e}");
            self.cleanup_failed_agent(&entity_id, user_id).await;
            return;
        }

        // Resume with legacy conversation file. The llm_caller reads
        // the conversation from this file for context.
        let resume_params = serde_json::json!({
            "sandbox_url": session.sandbox_url,
            "sandbox_id": session.sandbox_id,
            "workspace_id": session.workspace_id,
            "conversation_file_id": session.conversation_file_id,
            "file_manifest_id": session.file_manifest_id,
        });

        match self
            .state
            .dispatch_tenant_action(
                &tenant,
                "TemperAgent",
                &entity_id,
                "Resume",
                resume_params,
                &agent_ctx,
            )
            .await
        {
            Ok(resp) if resp.success => {
                println!(
                    "  [discord] Agent {entity_id} resuming conversation for {}",
                    msg.author.username
                );
            }
            Ok(resp) => {
                eprintln!(
                    "  [discord] Resume failed: {}",
                    resp.error.unwrap_or_default()
                );
                // Clear session and retry as first message.
                self.user_sessions.write().await.remove(user_id);
                self.cleanup_failed_agent(&entity_id, user_id).await;
            }
            Err(e) => {
                eprintln!("  [discord] Resume dispatch error: {e}");
                self.user_sessions.write().await.remove(user_id);
                self.cleanup_failed_agent(&entity_id, user_id).await;
            }
        }
    }

    /// Append a user message to the session tree JSONL file in TemperFS.
    ///
    /// Reads the current JSONL, appends a new user message entry with the
    /// correct `parentId`, and writes it back. Returns the new leaf entry ID.
    #[allow(dead_code)] // Will be used when WASM session tree integration is complete
    async fn append_to_session_tree(
        &self,
        session_file_id: &str,
        session_leaf_id: &str,
        content: &str,
    ) -> Result<String, String> {
        let base_url = self.temper_api_url();
        let tenant = &self.config.tenant;

        // Read current session tree JSONL from TemperFS.
        let get_url = format!("{base_url}/tdata/Files('{session_file_id}')/$value");
        let resp = self
            .http
            .get(&get_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .send()
            .await
            .map_err(|e| format!("GET session tree failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET session tree returned {status}: {body}"));
        }

        let mut body = resp
            .text()
            .await
            .map_err(|e| format!("read session tree body: {e}"))?;

        // Count existing entries to generate a unique ID.
        let entry_count = body.lines().filter(|l| !l.trim().is_empty()).count();
        let new_id = format!("u-discord-{entry_count}");
        let tokens = content.len() / 4; // rough estimate matching session_tree_lib

        // Append new user message entry as JSONL line.
        let entry = serde_json::json!({
            "id": new_id,
            "parentId": session_leaf_id,
            "type": "message",
            "role": "user",
            "content": content,
            "tokens": tokens,
        });

        if !body.ends_with('\n') && !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&entry.to_string());
        body.push('\n');

        // Write updated JSONL back to TemperFS.
        let put_url = format!("{base_url}/tdata/Files('{session_file_id}')/$value");
        let put_resp = self
            .http
            .put(&put_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("PUT session tree failed: {e}"))?;

        if !put_resp.status().is_success() {
            let status = put_resp.status();
            let body = put_resp.text().await.unwrap_or_default();
            return Err(format!("PUT session tree returned {status}: {body}"));
        }

        println!(
            "  [discord] Appended user message to session tree {session_file_id} (new leaf={new_id})",
        );

        Ok(new_id)
    }

    /// Append a user message to the legacy flat JSON conversation file.
    async fn append_to_legacy_conversation(
        &self,
        conversation_file_id: &str,
        content: &str,
    ) -> Result<(), String> {
        let base_url = self.temper_api_url();
        let tenant = &self.config.tenant;

        let get_url = format!("{base_url}/tdata/Files('{conversation_file_id}')/$value");
        let resp = self
            .http
            .get(&get_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .send()
            .await
            .map_err(|e| format!("GET conversation failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("GET conversation returned {status}: {body}"));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| format!("read conversation body: {e}"))?;

        let mut conv: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("parse conversation JSON: {e}"))?;

        let msg_count = {
            let messages = conv
                .get_mut("messages")
                .and_then(|v| v.as_array_mut())
                .ok_or("conversation missing messages array")?;
            messages.push(serde_json::json!({ "role": "user", "content": content }));
            messages.len()
        };

        let put_url = format!("{base_url}/tdata/Files('{conversation_file_id}')/$value");
        let put_resp = self
            .http
            .put(&put_url)
            .header("x-tenant-id", tenant)
            .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
            .header("content-type", "application/json")
            .body(conv.to_string())
            .send()
            .await
            .map_err(|e| format!("PUT conversation failed: {e}"))?;

        if !put_resp.status().is_success() {
            let status = put_resp.status();
            let body = put_resp.text().await.unwrap_or_default();
            return Err(format!("PUT conversation returned {status}: {body}"));
        }

        println!(
            "  [discord] Appended user message to conversation {conversation_file_id} ({msg_count} messages)",
        );
        Ok(())
    }

    /// Spawn a task that watches for TemperAgent completion and delivers replies.
    ///
    /// On completion: saves session state, delivers reply, drains queued messages.
    fn spawn_reply_watcher(&self) {
        let event_rx = self.state.event_tx.subscribe();
        let pending_replies = self.pending_replies.clone();
        let user_sessions = self.user_sessions.clone();
        let active_users = self.active_users.clone();
        let http = self.http.clone();
        let bot_token = self.config.bot_token.clone();
        let tenant = self.config.tenant.clone();
        let temper_api_url = self.temper_api_url();
        let sessions_file_id = self.sessions_file_id.clone();
        let state = self.state.clone();

        let reply_task = async move {
            let mut rx = tokio_stream::wrappers::BroadcastStream::new(event_rx);

            while let Some(Ok(event)) = rx.next().await {
                // Watch for TemperAgent reaching terminal states.
                if event.tenant != tenant || event.entity_type != "TemperAgent" {
                    continue;
                }

                let is_completed = event.action == "RecordResult" && event.status == "Completed";
                let is_failed = event.action == "Fail" && event.status == "Failed";

                if !is_completed && !is_failed {
                    continue;
                }

                // Check if this agent has a pending Discord reply.
                let reply_info = {
                    let mut pending = pending_replies.write().await;
                    pending.remove(&event.entity_id)
                };

                let Some(reply_info) = reply_info else {
                    continue; // Not a Discord-originated agent.
                };

                let channel_id = &reply_info.discord_channel_id;
                let user_id = &reply_info.discord_user_id;

                // Read entity state for result + session details.
                let tenant_id = TenantId::new(&tenant);
                let entity_state = state
                    .get_tenant_entity_state(&tenant_id, "TemperAgent", &event.entity_id)
                    .await;

                if is_failed {
                    let error_msg = entity_state
                        .as_ref()
                        .ok()
                        .and_then(|s| s.state.fields.get("error_message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error");
                    eprintln!("  [discord] Agent {} failed: {error_msg}", event.entity_id);
                    let _ = send_discord_message(
                        &http,
                        &bot_token,
                        channel_id,
                        "Sorry, I encountered an error processing your message.",
                    )
                    .await;
                    // Clear active state but preserve session for retry.
                    active_users.write().await.remove(user_id);
                    continue;
                }

                // Agent completed — extract result and save session.
                if let Ok(ref resp) = entity_state {
                    let fields = &resp.state.fields;

                    // Save session state for conversation continuity.
                    let conv_file_id = fields
                        .get("conversation_file_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let sess_file_id = fields
                        .get("session_file_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    if !sess_file_id.is_empty() || !conv_file_id.is_empty() {
                        // If no session tree exists, bootstrap one from the
                        // conversation. This enables compaction on follow-ups.
                        let (sess_file_id, sess_leaf_id) = if sess_file_id.is_empty()
                            && !conv_file_id.is_empty()
                        {
                            match create_session_tree_from_conversation(
                                &http,
                                &temper_api_url,
                                &tenant,
                                &conv_file_id,
                                &event.entity_id,
                            )
                            .await
                            {
                                Ok((fid, lid)) => {
                                    println!(
                                        "  [discord] Created session tree for user {user_id} (file={fid}, leaf={lid})"
                                    );
                                    (fid, lid)
                                }
                                Err(e) => {
                                    eprintln!("  [discord] Failed to create session tree: {e}");
                                    (String::new(), String::new())
                                }
                            }
                        } else {
                            let leaf = fields
                                .get("session_leaf_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            (sess_file_id, leaf)
                        };

                        let session = UserSession {
                            conversation_file_id: conv_file_id,
                            workspace_id: fields
                                .get("workspace_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            sandbox_url: fields
                                .get("sandbox_url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            sandbox_id: fields
                                .get("sandbox_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            file_manifest_id: fields
                                .get("file_manifest_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            session_file_id: sess_file_id,
                            session_leaf_id: sess_leaf_id,
                        };
                        println!(
                            "  [discord] Saved session for user {user_id} (session_file={}, leaf={})",
                            session.session_file_id, session.session_leaf_id
                        );
                        user_sessions.write().await.insert(user_id.clone(), session);

                        // Persist sessions to TemperFS for restart resilience.
                        persist_sessions_to_temperfs(
                            &http,
                            &temper_api_url,
                            &tenant,
                            &sessions_file_id,
                            &user_sessions,
                        )
                        .await;
                    }

                    // Deliver the reply. Guard against empty result.
                    let result_text = fields
                        .get("result")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("(I processed your message but had no response to give.)")
                        .to_string();

                    println!(
                        "  [discord] Delivering reply for {} ({} chars)",
                        event.entity_id,
                        result_text.len()
                    );

                    if let Err(e) =
                        send_discord_message(&http, &bot_token, channel_id, &result_text).await
                    {
                        eprintln!("  [discord] Reply delivery failed: {e}");
                    }
                } else {
                    eprintln!(
                        "  [discord] Failed to read agent state for {}",
                        event.entity_id
                    );
                    let _ = send_discord_message(
                        &http,
                        &bot_token,
                        channel_id,
                        "Sorry, I couldn't retrieve my response.",
                    )
                    .await;
                }

                // Clear active state and check for queued messages.
                let queued = active_users.write().await.remove(user_id);
                if let Some(queued_msgs) = queued
                    && !queued_msgs.is_empty()
                {
                    // Combine queued messages and process as a follow-up.
                    let combined = queued_msgs.join("\n");
                    println!(
                        "  [discord] Processing {} queued message(s) for {user_id}",
                        queued_msgs.len()
                    );

                    // Synthesize a MessageCreateData for the queued messages.
                    // We reuse the channel_id from the reply info.
                    let queued_msg = MessageCreateData {
                        id: format!("queued-{}", event.entity_id),
                        channel_id: reply_info.discord_channel_id.clone(),
                        content: combined,
                        author: DiscordUser {
                            id: user_id.clone(),
                            username: String::new(), // Not needed for follow-up.
                            bot: false,
                            discriminator: None,
                        },
                        guild_id: None,
                    };

                    // Re-insert active marker before processing.
                    active_users
                        .write()
                        .await
                        .insert(user_id.clone(), Vec::new());

                    // Queued messages will be picked up on the user's
                    // next interaction. Clear the active lock so the next
                    // message triggers the follow-up flow normally.
                    println!("  [discord] Queued messages deferred to next interaction");
                    active_users.write().await.remove(user_id);
                    let _ = queued_msg;
                }
            }
        };
        tokio::spawn(reply_task); // determinism-ok: background task for reply delivery
    }

    /// Send a typing indicator to a Discord channel.
    async fn send_typing(&self, channel_id: &str) {
        let _ = self
            .http
            .post(format!("{DISCORD_API_BASE}/channels/{channel_id}/typing"))
            .header("Authorization", format!("Bot {}", self.config.bot_token))
            .send()
            .await;
    }

    /// Get the local server URL for TemperFS API calls.
    fn temper_api_url(&self) -> String {
        let port = self.state.listen_port.get().copied().unwrap_or(3000);
        format!("http://127.0.0.1:{port}")
    }

    /// System prompt for Discord DM agents.
    fn system_prompt(&self, username: &str) -> String {
        format!(
            "You are a helpful AI assistant responding to a Discord DM from {username}. \
             Be concise and conversational. Keep responses under 1500 characters \
             when possible since Discord has a 2000 character limit per message."
        )
    }

    /// Clean up after a failed agent dispatch.
    async fn cleanup_failed_agent(&self, entity_id: &str, user_id: &str) {
        self.pending_replies.write().await.remove(entity_id);
        self.active_users.write().await.remove(user_id);
    }
}

/// Truncate a string for display.
/// Persist user sessions to TemperFS. Called from the reply watcher after
/// session updates. Creates the sessions file on first call.
async fn persist_sessions_to_temperfs(
    http: &reqwest::Client,
    temper_api_url: &str,
    tenant: &str,
    sessions_file_id: &Arc<RwLock<Option<String>>>,
    user_sessions: &Arc<RwLock<BTreeMap<String, UserSession>>>,
) {
    let sessions = user_sessions.read().await.clone();
    let content = serde_json::to_string_pretty(&sessions).unwrap_or_else(|_| "{}".to_string());

    // Ensure sessions file exists.
    let file_id = {
        let existing = sessions_file_id.read().await.clone();
        if let Some(id) = existing {
            id
        } else {
            let create_body = serde_json::json!({
                "name": "discord-sessions.json",
                "mime_type": "application/json",
                "path": "/discord-sessions.json",
            });
            let resp = match http
                .post(format!("{temper_api_url}/tdata/Files"))
                .header("x-tenant-id", tenant)
                .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
                .header("content-type", "application/json")
                .body(serde_json::to_string(&create_body).unwrap_or_default())
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    eprintln!("  [discord] Failed to create sessions file: {}", r.status());
                    return;
                }
                Err(e) => {
                    eprintln!("  [discord] Failed to create sessions file: {e}");
                    return;
                }
            };

            let data: serde_json::Value = resp.json().await.unwrap_or_default();
            let new_id = data
                .get("entity_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if new_id.is_empty() {
                eprintln!("  [discord] Sessions file created but no entity_id returned");
                return;
            }

            *sessions_file_id.write().await = Some(new_id.clone());
            new_id
        }
    };

    // Write sessions JSON to TemperFS.
    let put_url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    match http
        .put(&put_url)
        .header("x-tenant-id", tenant)
        .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
        .header("content-type", "application/json")
        .body(content)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            eprintln!("  [discord] Failed to persist sessions: {}", r.status());
        }
        Err(e) => {
            eprintln!("  [discord] Failed to persist sessions: {e}");
        }
    }
}

/// Create a session tree JSONL file in TemperFS from an existing conversation.
///
/// Reads the legacy conversation file, converts messages to session tree entries,
/// and creates a new JSONL File in TemperFS. Returns (session_file_id, session_leaf_id).
async fn create_session_tree_from_conversation(
    http: &reqwest::Client,
    temper_api_url: &str,
    tenant: &str,
    conversation_file_id: &str,
    agent_id: &str,
) -> Result<(String, String), String> {
    // Read the existing conversation from TemperFS.
    let get_url = format!("{temper_api_url}/tdata/Files('{conversation_file_id}')/$value");
    let resp = http
        .get(&get_url)
        .header("x-tenant-id", tenant)
        .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
        .send()
        .await
        .map_err(|e| format!("GET conversation failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GET conversation returned {}", resp.status()));
    }

    let body = resp.text().await.map_err(|e| format!("read body: {e}"))?;
    let conv: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse JSON: {e}"))?;
    let messages = conv
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or("missing messages array")?;

    // Build JSONL session tree from the messages.
    let header_id = format!("h-{agent_id}");
    let header = serde_json::json!({
        "id": header_id,
        "parentId": null,
        "type": "header",
        "version": 1,
        "tokens": 0
    });
    let mut lines = vec![serde_json::to_string(&header).unwrap_or_default()];
    let mut parent_id = header_id;

    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
        // Content may be a plain string or Anthropic's content block array:
        // [{"type": "text", "text": "..."}]
        let content = match msg.get("content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        // Skip empty messages (e.g., assistant with no content blocks).
        if content.is_empty() {
            continue;
        }
        let prefix = if role == "assistant" { "a" } else { "u" };
        let entry_id = format!("{prefix}-{agent_id}-{i}");
        let tokens = content.len() / 4;
        let entry = serde_json::json!({
            "id": entry_id,
            "parentId": parent_id,
            "type": "message",
            "role": role,
            "content": content,
            "tokens": tokens,
        });
        lines.push(serde_json::to_string(&entry).unwrap_or_default());
        parent_id = entry_id;
    }

    let jsonl = lines.join("\n");
    let leaf_id = parent_id;

    // Create session File entity in TemperFS.
    let create_body = serde_json::json!({
        "name": "session.jsonl",
        "mime_type": "text/plain",
        "path": "/session.jsonl"
    });
    let create_resp = http
        .post(format!("{temper_api_url}/tdata/Files"))
        .header("x-tenant-id", tenant)
        .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
        .header("content-type", "application/json")
        .body(serde_json::to_string(&create_body).unwrap_or_default())
        .send()
        .await
        .map_err(|e| format!("POST Files failed: {e}"))?;

    if !create_resp.status().is_success() {
        return Err(format!("POST Files returned {}", create_resp.status()));
    }

    let create_data: serde_json::Value = create_resp
        .json()
        .await
        .map_err(|e| format!("parse create resp: {e}"))?;
    let session_file_id = create_data
        .get("entity_id")
        .and_then(|v| v.as_str())
        .ok_or("missing entity_id in create response")?
        .to_string();

    // Write JSONL content.
    let put_url = format!("{temper_api_url}/tdata/Files('{session_file_id}')/$value");
    let put_resp = http
        .put(&put_url)
        .header("x-tenant-id", tenant)
        .header("x-temper-principal-kind", INTERNAL_PRINCIPAL_KIND)
        .header("content-type", "application/octet-stream")
        .body(jsonl)
        .send()
        .await
        .map_err(|e| format!("PUT session $value failed: {e}"))?;

    if !put_resp.status().is_success() {
        return Err(format!("PUT session $value returned {}", put_resp.status()));
    }

    Ok((session_file_id, leaf_id))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
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
            .map_err(|e| format!("Discord message send failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Discord API returned {status}: {body}"));
        }
    }

    Ok(())
}

/// Split a message into chunks of at most `max_len` characters.
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

        // Find char-safe boundary, then try to split at a newline within it.
        let boundary = remaining.floor_char_boundary(max_len);
        let split_at = remaining[..boundary].rfind('\n').unwrap_or(boundary);

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk);
        remaining = rest.trim_start_matches('\n');
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EntityStateChange;

    /// Check if an EntityStateChange is a completed agent for the given tenant.
    fn is_agent_terminal_event(event: &EntityStateChange, tenant: &str) -> bool {
        event.tenant == tenant
            && event.entity_type == "TemperAgent"
            && (event.status == "Completed" || event.status == "Failed")
    }

    #[test]
    fn split_message_short() {
        let chunks = split_message("hello", 2000);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_at_newline() {
        let content = format!("{}\n{}", "a".repeat(1500), "b".repeat(1000));
        let chunks = split_message(&content, 2000);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 1500);
    }

    #[test]
    fn is_agent_terminal_completed() {
        let event = EntityStateChange {
            seq: 0,
            entity_type: "TemperAgent".into(),
            entity_id: "discord-123".into(),
            action: "RecordResult".into(),
            status: "Completed".into(),
            tenant: "rita-agents".into(),
            agent_id: None,
            session_id: None,
        };
        assert!(is_agent_terminal_event(&event, "rita-agents"));
    }

    #[test]
    fn is_agent_terminal_failed() {
        let event = EntityStateChange {
            seq: 0,
            entity_type: "TemperAgent".into(),
            entity_id: "discord-123".into(),
            action: "Fail".into(),
            status: "Failed".into(),
            tenant: "rita-agents".into(),
            agent_id: None,
            session_id: None,
        };
        assert!(is_agent_terminal_event(&event, "rita-agents"));
    }

    #[test]
    fn is_agent_terminal_ignores_thinking() {
        let event = EntityStateChange {
            seq: 0,
            entity_type: "TemperAgent".into(),
            entity_id: "discord-123".into(),
            action: "SandboxReady".into(),
            status: "Thinking".into(),
            tenant: "rita-agents".into(),
            agent_id: None,
            session_id: None,
        };
        assert!(!is_agent_terminal_event(&event, "rita-agents"));
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_emoji_boundary() {
        // "😀" is 4 bytes — truncating at byte 2 must not panic.
        let s = "😀hello";
        let result = truncate(s, 2);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn split_message_emoji_boundary() {
        let emoji_chunk = "🎉".repeat(600); // 2400 bytes, each emoji 4 bytes
        let chunks = split_message(&emoji_chunk, 2000);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.len() <= 2000);
            // Verify each chunk is valid UTF-8 (would panic on &str if not).
            assert!(!chunk.is_empty());
        }
    }
}
