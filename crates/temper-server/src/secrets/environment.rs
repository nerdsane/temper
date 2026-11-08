//! Platform secrets supplied through the server's environment.
//!
//! An operator can hand the server any number of named secrets at start by
//! setting `TEMPER_SECRET_<NAME>` variables. Each one with a non-empty value
//! becomes the platform secret `<name>` in lower case: `TEMPER_SECRET_BUILD_TOKEN`
//! supplies `build_token`. A platform secret is the baseline for every tenant;
//! a tenant's own stored secret of the same name overrides it for that tenant.
//!
//! `<NAME>` is one or more of `A` to `Z`, `0` to `9` and `_`, starting with a
//! letter, so that no two variables can supply the same secret. A variable
//! that does not fit, or whose value is larger than the secrets API accepts,
//! is skipped and reported by name. Values are never reported. A name the
//! server already holds a platform secret for is left as it is, so what the
//! server sets for itself at start wins.
//!
//! A seeded secret is read like any other secret a tenant can read: a
//! module's `get_secret` call needs a policy that permits `access_secret` on
//! it, and a `{secret:<name>}` template in an integration config is resolved
//! without that check. Supply this way only what every tenant's specs may use.
//!
//! Nothing here reads the process environment or writes to storage: the
//! caller passes the variables in, and the secrets live in the vault's
//! in-memory platform layer only.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::vault::{MAX_SECRET_VALUE_BYTES, SecretsVault};

/// Prefix of an environment variable that supplies a platform secret.
pub const ENVIRONMENT_SECRET_PREFIX: &str = "TEMPER_SECRET_";

/// Why a prefixed environment variable did not become a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentSecretSkip {
    /// The part after the prefix does not fit the naming rule.
    InvalidName,
    /// The value is not valid UTF-8.
    ValueNotUnicode,
    /// The value is larger than the secrets API accepts.
    ValueTooLarge,
    /// The server already holds a platform secret of that name.
    AlreadySet,
}

impl EnvironmentSecretSkip {
    /// A short description for the start-up log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidName => {
                "the name after the prefix must be A-Z, 0-9 or _, starting with a letter"
            }
            Self::ValueNotUnicode => "the value is not valid UTF-8",
            Self::ValueTooLarge => "the value is larger than the maximum size of a secret",
            Self::AlreadySet => "the server already sets a secret of that name",
        }
    }
}

/// What seeding from the environment did. Holds names only, never values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentSecretsReport {
    /// Names of the secrets that were seeded, in variable-name order.
    pub seeded: Vec<String>,
    /// Variables that were skipped, by variable name, with the reason.
    pub skipped: Vec<(String, EnvironmentSecretSkip)>,
}

impl EnvironmentSecretsReport {
    /// Log each skipped variable once by name, and how many secrets were
    /// seeded. Logs nothing when no prefixed variable was set.
    fn log(&self) {
        for (variable, reason) in &self.skipped {
            tracing::warn!(
                variable = %variable,
                reason = reason.as_str(),
                "environment variable was not seeded as a secret"
            );
        }
        if !self.seeded.is_empty() {
            tracing::info!(
                count = self.seeded.len(),
                "seeded platform secrets from TEMPER_SECRET_ environment variables"
            );
        }
    }
}

/// Seed the vault's platform layer from `TEMPER_SECRET_<NAME>` variables.
///
/// `variables` is the environment as `(name, value)` pairs; anything without
/// the prefix, and any prefixed variable with an empty value, is ignored.
/// Variables are taken in name order, so the outcome does not depend on the
/// order the environment lists them in. The outcome is logged (names and a
/// count, never values) and returned.
pub fn seed_platform_secrets_from_environment<I>(
    vault: &SecretsVault,
    variables: I,
) -> EnvironmentSecretsReport
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let prefixed: BTreeMap<OsString, OsString> = variables
        .into_iter()
        .filter(|(variable, value)| has_secret_prefix(variable) && !value.is_empty())
        .collect();

    let mut report = EnvironmentSecretsReport::default();
    for (variable, value) in prefixed {
        match seed_one(vault, &variable, value) {
            Ok(secret) => report.seeded.push(secret),
            Err(reason) => report
                .skipped
                .push((variable.to_string_lossy().into_owned(), reason)),
        }
    }

    debug_assert!(
        report
            .seeded
            .iter()
            .all(|secret| vault.get_platform_secret(secret).is_some()),
        "every secret reported as seeded must be in the platform layer"
    );
    report.log();
    report
}

fn has_secret_prefix(variable: &OsStr) -> bool {
    variable
        .as_encoded_bytes()
        .starts_with(ENVIRONMENT_SECRET_PREFIX.as_bytes())
}

/// The secret a prefixed variable names, or `None` when the part after the
/// prefix does not fit the naming rule.
fn secret_name(variable: &OsStr) -> Option<String> {
    let name = variable.to_str()?.strip_prefix(ENVIRONMENT_SECRET_PREFIX)?;
    let mut chars = name.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_uppercase());
    let rest_fits = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    (starts_with_letter && rest_fits).then(|| name.to_ascii_lowercase())
}

fn seed_one(
    vault: &SecretsVault,
    variable: &OsStr,
    value: OsString,
) -> Result<String, EnvironmentSecretSkip> {
    let secret = secret_name(variable).ok_or(EnvironmentSecretSkip::InvalidName)?;
    // The rejected value is dropped here, not carried in the error.
    let value = value
        .into_string()
        .map_err(|_| EnvironmentSecretSkip::ValueNotUnicode)?;
    if value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(EnvironmentSecretSkip::ValueTooLarge);
    }
    if vault.get_platform_secret(&secret).is_some() {
        return Err(EnvironmentSecretSkip::AlreadySet);
    }
    vault.cache_platform_secret(&secret, value);
    Ok(secret)
}

#[cfg(test)]
mod tests;
