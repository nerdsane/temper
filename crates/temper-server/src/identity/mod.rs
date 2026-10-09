//! Agent identity resolution — platform-assigned identity from credentials.
//!
//! Agents present bearer tokens; the platform resolves them to verified identities
//! by looking up `AgentCredential` entities and their linked `AgentType`.
//! See ADR-0033: Platform-Assigned Agent Identity.

pub mod endpoint;
mod error;
pub mod jwt;
mod resolver;
mod resolver_support;

pub use error::IdentityError;
pub use resolver::{IdentityResolver, MAX_CREDENTIAL_BYTES, ResolvedIdentity};
pub use resolver_support::hash_token;
