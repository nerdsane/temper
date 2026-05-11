//! Transport-agnostic bus messages for the actor system.
//!
//! Actors that need to communicate over an external bus (Zenoh, Nexus, etc.)
//! use these generic messages. The BusActor implementation owns the transport
//! details — callers only pass semantic parameters.
//!
//! - `StreamMsg`  — fire-and-forget data to session subscribers (token streaming)
//! - `CallMsg`    — request-response to a session/client handler (tool dispatch)
//! - `CallReply`  — reply to a CallMsg

/// Fire-and-forget: push data to all subscribers on this session.
///
/// The BusActor maps session_id to the appropriate topic/address
/// (e.g. `session/{session_id}/tokens` in Zenoh).
#[derive(Clone, PartialEq, prost::Message)]
pub struct StreamMsg {
    /// Session identifier (extracted from actor namespace).
    #[prost(string, tag = "1")]
    pub session_id: String,
    /// Payload bytes (e.g. serialized token chunk or full response).
    #[prost(bytes, tag = "2")]
    pub payload: Vec<u8>,
}

/// Request-response: invoke a specific client handler on this session.
///
/// The BusActor maps (session_id, client_id) to the appropriate address
/// (e.g. `session/{session_id}/{client_id}/tools` in Zenoh).
#[derive(Clone, PartialEq, prost::Message)]
pub struct CallMsg {
    /// Session identifier.
    #[prost(string, tag = "1")]
    pub session_id: String,
    /// Client identifier (the specific client to call).
    #[prost(string, tag = "2")]
    pub client_id: String,
    /// Request payload (e.g. serialized tool call).
    #[prost(bytes, tag = "3")]
    pub payload: Vec<u8>,
}

/// Reply to a CallMsg — sent back by the BusActor after the remote handler responds.
#[derive(Clone, PartialEq, prost::Message)]
pub struct CallReply {
    /// Response payload from the remote handler.
    #[prost(bytes, tag = "1")]
    pub payload: Vec<u8>,
}

/// Conventional actor type name for the Bus actor.
pub const BUS_ACTOR_TYPE: &str = "Bus";
