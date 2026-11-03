//! `[[webhook]]`: a request another system sends in, which becomes one action
//! on one entity (ADR-0182).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An inbound webhook declaration.
///
/// A provider (Stripe, GitHub, a Standard Webhooks sender such as Svix, or
/// any system that signs with a shared secret) calls
/// `/webhooks/{tenant}/{path}`. The receiver checks the request's signature
/// with the provider's own scheme, reads the target entity's id and the
/// action's parameters from the request, and dispatches `action` as
/// `Agent::"webhook:{name}"`, which the tenant's Cedar must permit. Webhooks
/// are metadata only: they do not affect verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    /// Webhook name (e.g., "stripe_paid"); the Cedar principal is
    /// `Agent::"webhook:{name}"`.
    pub name: String,
    /// URL path suffix (e.g., "stripe/paid").
    pub path: String,
    /// The one HTTP method this webhook answers. Absent: any method.
    #[serde(default)]
    pub method: Option<String>,
    /// Action to dispatch when the webhook is called.
    pub action: String,
    /// Where the request carries the target entity's id, as a
    /// [`WebhookSource`]: `query.<name>`, `header.<name>`, `body` or
    /// `body.<path>`.
    pub entity_id: String,
    /// Action parameters read from the request: parameter name to source, in
    /// the same forms as `entity_id`.
    #[serde(default)]
    pub extract: BTreeMap<String, String>,
    /// How the sender signs the request. Default: `kernel`.
    #[serde(default)]
    pub signature: WebhookSignature,
    /// The signing secret the sender shares, normally a `{secret:KEY}`
    /// template. Required: the receiver admits only signed requests.
    #[serde(default)]
    pub hmac_secret: Option<String>,
    /// The header that carries the signature, for the `kernel`, `hmac` and
    /// `token` schemes. The other schemes use the header their provider
    /// fixes.
    #[serde(default)]
    pub hmac_header: Option<String>,
}

/// A provider's signature scheme. Every scheme but `token` is HMAC-SHA256
/// with the webhook's `hmac_secret`; they differ in what is signed and where
/// the signature travels.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSignature {
    /// Temper's own scheme: hex HMAC of `METHOD\nPATH?QUERY\nBODY` in
    /// `hmac_header`, optionally prefixed `sha256=`.
    #[default]
    Kernel,
    /// HMAC of the raw body in `hmac_header`, hex or base64, optionally
    /// prefixed `sha256=` (Shopify, Linear, Gitea and most others).
    Hmac,
    /// GitHub: `X-Hub-Signature-256: sha256=<hex HMAC of the body>`.
    Github,
    /// Stripe: `Stripe-Signature: t=<unix seconds>,v1=<hex>`, the HMAC of
    /// `<t>.<body>`, with the timestamp checked against replay.
    Stripe,
    /// Standard Webhooks (Svix): `webhook-id`, `webhook-timestamp` and
    /// `webhook-signature: v1,<base64>` (or the `svix-` spellings), the HMAC
    /// of `<id>.<timestamp>.<body>` keyed by the base64 secret after
    /// `whsec_`, with the timestamp checked against replay.
    StandardWebhooks,
    /// The secret itself in `hmac_header`, for senders that cannot sign
    /// (GitLab's `X-Gitlab-Token`, a custom header on a Datadog webhook).
    /// Weaker than a signature: it does not cover the body or the time.
    Token,
}

impl WebhookSignature {
    /// Spec spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kernel => "kernel",
            Self::Hmac => "hmac",
            Self::Github => "github",
            Self::Stripe => "stripe",
            Self::StandardWebhooks => "standard_webhooks",
            Self::Token => "token",
        }
    }

    /// Whether the declaration names the signature header (`hmac_header`).
    /// The other schemes use the header their provider fixes.
    pub fn names_its_header(self) -> bool {
        matches!(self, Self::Kernel | Self::Hmac | Self::Token)
    }
}

/// Where a webhook reads one value from the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookSource<'a> {
    /// `query.<name>`: a query-string parameter.
    Query(&'a str),
    /// `header.<name>`: a request header (names are case-insensitive).
    Header(&'a str),
    /// `body`: the whole JSON body (an empty path), or `body.<path>`: the
    /// value at a dot-separated path into it, where a number indexes an
    /// array (`body.data.object.id`, `body.items.0.sku`).
    Body(&'a str),
}

impl<'a> WebhookSource<'a> {
    /// Read a source spelling; `None` for anything else.
    pub fn parse(source: &'a str) -> Option<Self> {
        if source == "body" {
            return Some(Self::Body(""));
        }
        let (kind, rest) = source.split_once('.')?;
        if rest.is_empty() {
            return None;
        }
        match kind {
            "query" => Some(Self::Query(rest)),
            "header" => Some(Self::Header(rest)),
            "body" if rest.split('.').all(|segment| !segment.is_empty()) => Some(Self::Body(rest)),
            _ => None,
        }
    }
}
