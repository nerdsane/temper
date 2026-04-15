//! Discord channel transport — Gateway WebSocket + REST API.
//!
//! Connects to Discord's Gateway (wss://gateway.discord.gg), receives
//! MESSAGE_CREATE events, and dispatches them as Channel.ReceiveMessage
//! actions via the Temper OData API. Watches for Channel.SendReply events
//! and delivers replies via Discord's REST API.
//!
//! This is a Temper OData API client — no dependency on temper-server internals.

pub mod types;

mod gateway;
mod transport;

pub use gateway::send_discord_message;
pub use transport::{DiscordConfig, DiscordTransport};
