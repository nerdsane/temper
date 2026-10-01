//! Identity that the host process embedding the platform router verified.

use temper_authz::SecurityContext;

/// Principal a host process verified before the platform router, e.g. from
/// a token the platform cannot interpret. Request extensions cannot be set
/// over HTTP; a host inserts this in its own middleware. It authenticates
/// the request in its requested tenant; tenant policy still authorizes it.
#[derive(Clone, Debug)]
pub struct HostVerifiedIdentity(pub SecurityContext);
