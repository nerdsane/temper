//! One log event for every use of the operator key.
//!
//! The operator key (`TEMPER_API_KEY`, and any other credential issued to the
//! tenant's operator) can do anything in its tenant. Every request whose
//! credential is checked against it, accepted or refused, logs one event with
//! `evt = "key_use"`, so whoever reads the logs can tell who used the key and
//! from where. The event never carries the key, any part of it or its hash,
//! and never the caller's address: the caller is the first 12 hex characters
//! of `sha256(KEY_USE_SALT + address)`.

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::extract::{ConnectInfo, MatchedPath};
use axum::http::{HeaderMap, Request, header};
use sha2::{Digest, Sha256};
use temper_server::identity::ResolvedIdentity;

/// Environment variable holding the salt for the caller hash.
pub const SALT_ENV: &str = "KEY_USE_SALT";

/// `key_name` of the credential `TEMPER_API_KEY` holds, and of a refused attempt.
pub const OPERATOR: &str = "operator";

/// `key_name` of another Active credential issued to the operator, such as an
/// older key that was never retired.
pub const OTHER_OPERATOR: &str = "operator_other";

/// `route` when the router names no template for the request.
const UNMATCHED_ROUTE: &str = "unmatched";

/// An operator key is 32 random bytes in hex.
const OPERATOR_KEY_HEX_LEN: usize = 64;

const CALLER_HASH_HEX_LEN: usize = 12;

/// Whether the presented key matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The credential is the operator's.
    Accepted,
    /// A credential offered as the operator key, or shaped like one, did not match.
    Refused,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Refused => "refused",
        }
    }
}

/// One use of the operator key, as it is logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyUse {
    /// Whether the key matched.
    pub outcome: Outcome,
    /// Which operator key matched: [`OPERATOR`] or [`OTHER_OPERATOR`].
    pub key_name: &'static str,
    /// The route template, never the raw path.
    pub route: String,
    /// The HTTP method.
    pub method: String,
    /// Salted hash of the caller's address, `unsalted` or `unknown`.
    pub caller: String,
    /// The User-Agent's family: node, curl, python, browser or other.
    pub agent: &'static str,
}

impl KeyUse {
    /// The request's credential `token` resolved to the operator.
    /// `configured_key` is the deployment's `TEMPER_API_KEY`, which tells
    /// the key it holds apart from any other operator credential.
    pub fn accepted<B>(request: &Request<B>, token: &str, configured_key: Option<&str>) -> Self {
        let key_name = if configured_key == Some(token) {
            OPERATOR
        } else {
            OTHER_OPERATOR
        };
        Self::from_request(request, Outcome::Accepted, key_name, salt())
    }

    /// The request's credential was offered as the operator key and did not match.
    pub fn refused<B>(request: &Request<B>) -> Self {
        Self::from_request(request, Outcome::Refused, OPERATOR, salt())
    }

    fn from_request<B>(
        request: &Request<B>,
        outcome: Outcome,
        key_name: &'static str,
        salt: Option<&str>,
    ) -> Self {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| peer.ip());
        let user_agent = request
            .headers()
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        Self {
            outcome,
            key_name,
            route: request
                .extensions()
                .get::<MatchedPath>()
                .map_or(UNMATCHED_ROUTE, MatchedPath::as_str)
                .to_string(),
            method: request.method().as_str().to_string(),
            caller: caller(salt, caller_address(request.headers(), peer)),
            agent: agent_family(user_agent),
        }
    }

    /// Name the route by the HttpEndpoint row that serves it, when the router
    /// has no template for it.
    pub fn at_endpoint(mut self, path_prefix: Option<&str>) -> Self {
        if let Some(path_prefix) = path_prefix
            && self.route == UNMATCHED_ROUTE
        {
            self.route = path_prefix.to_string();
        }
        self
    }

    /// Log the event. Its fields become the log record's attributes.
    pub fn log(&self) {
        tracing::info!(
            evt = "key_use",
            outcome = self.outcome.as_str(),
            key = "operator",
            key_name = self.key_name,
            route = self.route.as_str(),
            method = self.method.as_str(),
            caller = self.caller.as_str(),
            country = "",
            agent = self.agent,
            "key_use"
        );
    }
}

/// Whether a resolved credential is the tenant's operator, as
/// `bootstrap_operator_credential` issues it. A trusted issuer's token (a
/// person, a computer) is never the operator.
pub fn is_operator(identity: &ResolvedIdentity) -> bool {
    !identity.from_jwt && identity.agent_type_name == "operator"
}

/// Whether an unresolved credential is shaped like an operator key.
pub fn looks_like_operator_key(token: &str) -> bool {
    token.len() == OPERATOR_KEY_HEX_LEN && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn salt() -> Option<&'static str> {
    static SALT: OnceLock<Option<String>> = OnceLock::new();
    SALT.get_or_init(|| {
        std::env::var(SALT_ENV) // determinism-ok: startup config, read once
            .ok()
            .filter(|salt| !salt.is_empty())
    })
    .as_deref()
}

/// The client the request came from: the address Railway's edge saw
/// (`X-Real-IP`), else the first `X-Forwarded-For` entry another proxy
/// wrote, else the TCP peer.
fn caller_address(headers: &HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
    let header_address = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .and_then(parse_address)
    };
    header_address("x-real-ip")
        .or_else(|| header_address("x-forwarded-for"))
        .or(peer)
}

fn parse_address(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    value
        .parse::<IpAddr>()
        .ok()
        .or_else(|| value.parse::<SocketAddr>().ok().map(|address| address.ip()))
}

fn caller(salt: Option<&str>, address: Option<IpAddr>) -> String {
    let Some(salt) = salt else {
        return "unsalted".to_string();
    };
    let Some(address) = address else {
        return "unknown".to_string();
    };
    let digest = Sha256::new()
        .chain_update(salt.as_bytes())
        .chain_update(address.to_string().as_bytes())
        .finalize();
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hash[..CALLER_HASH_HEX_LEN].to_string()
}

/// The User-Agent's family. Node's own fetch (undici), node-fetch and axios
/// are node; any `Mozilla/` agent is a browser.
fn agent_family(user_agent: &str) -> &'static str {
    let user_agent = user_agent.trim().to_ascii_lowercase();
    if user_agent.starts_with("curl/") {
        "curl"
    } else if user_agent.contains("python") {
        "python"
    } else if ["node", "undici", "axios"]
        .iter()
        .any(|family| user_agent.contains(family))
    {
        "node"
    } else if user_agent.starts_with("mozilla/") {
        "browser"
    } else {
        "other"
    }
}

#[cfg(test)]
#[path = "key_use/key_use_test.rs"]
mod tests;
