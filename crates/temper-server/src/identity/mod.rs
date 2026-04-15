//! Agent identity resolution — platform-assigned identity from credentials.
//!
//! Agents present bearer tokens; the platform resolves them to verified identities
//! by looking up `AgentCredential` entities and their linked `AgentType`.
//! See ADR-0033: Platform-Assigned Agent Identity.

pub mod endpoint;
mod resolver;

pub use resolver::{IdentityResolver, ResolvedIdentity, hash_token};
