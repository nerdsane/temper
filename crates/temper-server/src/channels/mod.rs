//! Channel transports: persistent connections to external messaging platforms.
//!
//! A channel transport bridges an external platform (Discord, Slack, etc.) to
//! Temper's Channel entity. It handles all platform-specific I/O:
//!
//! - **Inbound**: receives platform events (e.g., Discord MESSAGE_CREATE) and
//!   dispatches `ReceiveMessage` actions on Channel entities.
//! - **Outbound**: watches for `SendReply` state changes on Channel entities
//!   and delivers replies via the platform's API.
//!
//! Specs and WASM modules remain platform-agnostic — they never call
//! platform-specific APIs. Adding a new platform means adding one Rust file
//! here, not touching any specs or WASM.

pub mod discord;
pub mod discord_types;
