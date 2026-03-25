//! Discord channel transport — Gateway WebSocket + REST API.
//!
//! Connects to Discord's Gateway (wss://gateway.discord.gg), receives
//! MESSAGE_CREATE events, and dispatches them as Channel.ReceiveMessage
//! actions via the Temper OData API. Watches for Channel.SendReply events
//! and delivers replies via Discord's REST API.

pub mod types;

// Transport implementation will be migrated here from
// temper-server/src/channels/discord.rs in the next phase.
