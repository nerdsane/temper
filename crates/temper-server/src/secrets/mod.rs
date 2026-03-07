//! Tenant secret management: encrypted storage and template resolution.

pub mod template;
pub mod vault;

pub use template::resolve_secret_templates;
pub use vault::SecretsVault;
