//! Discord channel-transport startup for `temper serve`.

use temper_platform::state::PlatformState;

/// Spawn the Discord channel transport using the temper-transport crate.
///
/// The transport is an OData API client — it bootstraps Channel + AgentRoute
/// entities on startup, dispatches Channel.ReceiveMessage for inbound messages,
/// and receives replies via a webhook listener that send_reply WASM calls.
pub(super) fn spawn_channel_transport_discord(
    _state: &PlatformState,
    bot_token: String,
    tenant: &str,
    port: u16,
    api_key: Option<String>,
) {
    use temper_transport::discord::types::intents;
    use temper_transport::discord::{DiscordConfig, DiscordTransport};

    let tenant = tenant.to_string();
    let api_url = format!("http://127.0.0.1:{port}");
    println!("  Discord channel transport (v2): connecting (tenant={tenant})...");
    tokio::spawn(async move {
        // determinism-ok: WebSocket for channel transport
        let builder = temper_sdk::TemperClient::builder()
            .base_url(&api_url)
            .tenant(&tenant);
        // Without an API key the transport authenticates as the local
        // admin principal (matches server-side bearer-auth fallback).
        let builder = match api_key.as_deref() {
            Some(key) => builder.api_key(key),
            None => builder.principal_kind("admin"),
        };
        let api = match builder.build() {
            Ok(client) => client,
            Err(e) => {
                eprintln!("  [discord] Failed to build Temper API client: {e}");
                return;
            }
        };
        let config = DiscordConfig {
            bot_token,
            intents: intents::DEFAULT,
            webhook_port: 0, // Auto-assign
        };
        let transport = DiscordTransport::new(config, api);
        if let Err(e) = transport.run().await {
            eprintln!("  [discord] Transport fatal error: {e}");
        }
    });
}
