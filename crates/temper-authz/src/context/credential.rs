//! What a verified token was issued under, kept beside the identity it
//! resolved to.
//!
//! The request path refuses a token whose user signed out everywhere after it
//! was issued, or whose grant was revoked. A move made later in the
//! identity's name, after the token itself is gone (a timer an agent set),
//! asks the same question of what is kept here. Cedar never sees it: the
//! engine builds its request from the principal and `context_attrs` only.

use serde::{Deserialize, Serialize};

/// The facts about a verified token that decide whether it would still be
/// accepted: the user it was issued for, that user's sign-out generation when
/// it was issued, and the grant it was issued under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCredential {
    /// The token's `sub`: the user, or the user an agent acts for. Signing
    /// that user out everywhere advances their generation past
    /// [`Self::auth_generation`]. None when the token named no user.
    pub subject: Option<String>,
    /// The user's sign-out generation stamped into the token (0 when the
    /// issuer stamped none).
    pub auth_generation: i64,
    /// The grant the token was issued under, when it names one. Revoking the
    /// grant advances its generation past 0.
    pub grant_id: Option<String>,
}
