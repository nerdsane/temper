//! One write of an encrypted tenant secret, shared by every metadata store.

/// What a secret write requires of the key's current row.
///
/// A store checks the requirement and writes in one statement, so two writers
/// racing on one key can never both pass the same check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretPrecondition<'a> {
    /// Write whatever the key holds now.
    Any,
    /// Write only if the key has no row.
    Absent,
    /// Write only if the key's row carries exactly this version id.
    Version(&'a str),
}

/// An encrypted secret to store under `(tenant, key_name)`.
#[derive(Debug, Clone, Copy)]
pub struct SecretWrite<'a> {
    /// The tenant that owns the secret.
    pub tenant: &'a str,
    /// The secret's name within the tenant.
    pub key_name: &'a str,
    /// The AES-256-GCM ciphertext of the value.
    pub ciphertext: &'a [u8],
    /// The nonce the ciphertext was sealed with.
    pub nonce: &'a [u8],
    /// The id this write gives the row. It is a fresh unique id on every
    /// write, never derived from the value or ciphertext: a caller who may read
    /// version ids must not be able to test guesses of a low-entropy secret
    /// against them, and a key deleted and created again must not get back an
    /// id an old caller still holds.
    pub version_id: &'a str,
    /// What the key's current row must be for the write to happen.
    pub precondition: SecretPrecondition<'a>,
}
