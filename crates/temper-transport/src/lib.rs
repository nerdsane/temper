//! Platform-agnostic channel transport runtime for Temper.
//!
//! Transports bridge external messaging platforms (Discord, Slack, etc.) to
//! Temper's Channel entity architecture. Each transport drives the Temper
//! OData API through [`temper_sdk::TemperClient`] — it dispatches
//! `Channel.ReceiveMessage` for inbound messages and delivers outbound
//! replies via the platform's REST API.
//!
//! No dependency on temper-server internals. Communicates via HTTP only.

pub mod discord;
