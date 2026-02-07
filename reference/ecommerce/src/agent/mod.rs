//! LLM-powered agent for operating the e-commerce API.
//!
//! The agent reads $metadata to discover available actions, then uses
//! Claude to interpret natural language requests and execute OData operations.

pub mod client;
pub mod claude;
pub mod orchestrator;

pub use orchestrator::CustomerAgent;
