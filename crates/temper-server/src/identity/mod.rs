//! Agent identity resolution — platform-assigned identity from credentials.
//!
//! Agents present bearer tokens; the platform resolves them to verified identities
//! by looking up `AgentCredential` entities and their linked `AgentType`.
//! See ADR-0033: Platform-Assigned Agent Identity.

pub mod endpoint;
pub mod jwt;
mod resolver;

pub use resolver::{IdentityResolver, MAX_CREDENTIAL_BYTES, ResolvedIdentity, hash_token};
