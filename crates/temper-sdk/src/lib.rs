//! Temper SDK — thin HTTP client for Temper entity operations.
//!
//! Provides [`TemperClient`] with builder-pattern configuration for interacting
//! with a Temper server's OData entity endpoints, governance API, and SSE event
//! stream. Mirrors the dispatch surface exposed by `temper-mcp`.
//!
//! # Example
//!
//! ```no_run
//! use temper_sdk::TemperClient;
//! use serde_json::json;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let client = TemperClient::builder()
//!     .base_url("http://127.0.0.1:4200")
//!     .tenant("default")
//!     .build()?;
//!
//! let tasks = client.list("Tasks").await?;
//! let task = client.create("Tasks", json!({"id": "t-1"})).await?;
//! let updated = client.action("Tasks", "t-1", "Start", json!({})).await?;
//! # Ok(())
//! # }
//! ```

mod client;
mod sse;
mod types;

pub use client::{ClientBuilder, TemperClient};
pub use sse::parse_sse_stream;
pub use types::{AuditEntry, AuthzResponse, EntityEvent};
