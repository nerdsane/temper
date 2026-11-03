//! Provider signature schemes for inbound webhooks (ADR-0182).
//!
//! A webhook endpoint is public: without a signature anyone could send a
//! provider's events (a forged "payment succeeded"). Each scheme here is the
//! one its provider documents, keyed by the webhook's shared secret. A request
//! is admitted when one signature it carries matches; MACs are compared in
//! constant time by `hmac::Mac::verify_slice`.

use std::fmt;

use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use temper_spec::automaton::WebhookSignature;

type HmacSha256 = Hmac<Sha256>;

/// How far a signed timestamp may be from now, either way. Replaying a
/// captured request later than this is refused. Five minutes is the default
/// of Stripe's libraries (docs.stripe.com/webhooks, "Preventing replay
/// attacks") and of the Standard Webhooks reference libraries
/// (`TOLERANCE_IN_SECONDS`); the sender's and this server's clocks must agree
/// within it.
pub(crate) const SIGNATURE_TOLERANCE_SECONDS: i64 = 5 * 60;

/// The request as its sender signed it.
pub(crate) struct SignedRequest<'a> {
    /// HTTP method, as received.
    pub method: &'a str,
    /// Path and query, as received (`/webhooks/{tenant}/{path}?{query}`).
    pub path_and_query: &'a str,
    /// Request headers.
    pub headers: &'a HeaderMap,
    /// The raw body, unparsed.
    pub body: &'a [u8],
}

/// Why a request was not admitted. Logged for the operator; the sender only
/// sees 401.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// A header the scheme reads is absent or not text.
    MissingHeader(String),
    /// A header is present but not in the scheme's format.
    Malformed(&'static str),
    /// The signed timestamp is outside [`SIGNATURE_TOLERANCE_SECONDS`].
    Stale {
        /// Seconds from the signed timestamp to now (negative: in the future).
        age_seconds: i64,
    },
    /// The secret cannot be used as this scheme's key.
    UnusableSecret(&'static str),
    /// No signature matches.
    Mismatch,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader(name) => write!(f, "missing header {name}"),
            Self::Malformed(what) => write!(f, "malformed {what}"),
            Self::Stale { age_seconds } => write!(
                f,
                "signed {age_seconds}s from now, beyond {SIGNATURE_TOLERANCE_SECONDS}s"
            ),
            Self::UnusableSecret(why) => write!(f, "secret unusable: {why}"),
            Self::Mismatch => write!(f, "no signature matches"),
        }
    }
}

/// Check `request` against `scheme` with `secret`. `header` is the
/// declaration's `hmac_header` (the `kernel` and `hmac` schemes read it);
/// `now_unix_seconds` is the server's clock.
///
/// # Errors
///
/// Returns the [`Rejection`] that kept the request out.
pub(crate) fn verify_signature(
    scheme: WebhookSignature,
    secret: &str,
    header: Option<&str>,
    request: &SignedRequest<'_>,
    now_unix_seconds: i64,
) -> Result<(), Rejection> {
    if secret.is_empty() {
        return Err(Rejection::UnusableSecret("empty"));
    }
    let named = || header.ok_or_else(|| Rejection::MissingHeader("hmac_header".to_string()));
    match scheme {
        WebhookSignature::Kernel => {
            let provided = header_text(request.headers, named()?)?;
            let mut signed = Vec::with_capacity(
                request.method.len() + request.path_and_query.len() + request.body.len() + 2,
            );
            signed.extend_from_slice(request.method.as_bytes());
            signed.push(b'\n');
            signed.extend_from_slice(request.path_and_query.as_bytes());
            signed.push(b'\n');
            signed.extend_from_slice(request.body);
            matches_any(secret.as_bytes(), &signed, [decode_mac(provided)])
        }
        WebhookSignature::Hmac => {
            let provided = header_text(request.headers, named()?)?;
            matches_any(secret.as_bytes(), request.body, [decode_mac(provided)])
        }
        WebhookSignature::Github => {
            let provided = header_text(request.headers, "x-hub-signature-256")?;
            matches_any(secret.as_bytes(), request.body, [decode_mac(provided)])
        }
        WebhookSignature::Stripe => verify_stripe(secret, request, now_unix_seconds),
        WebhookSignature::StandardWebhooks => verify_standard(secret, request, now_unix_seconds),
        WebhookSignature::Token => {
            // The header carries the secret itself. Compared as MACs so the
            // comparison is constant-time: equal MACs mean equal tokens.
            let provided = header_text(request.headers, named()?)?;
            let expected = mac(secret.as_bytes(), secret.as_bytes())
                .finalize()
                .into_bytes();
            mac(secret.as_bytes(), provided.as_bytes())
                .verify_slice(&expected)
                .map_err(|_| Rejection::Mismatch)
        }
    }
}

/// Stripe: `Stripe-Signature: t=<unix>,v1=<hex>[,v1=<hex>...]` signs
/// `<t>.<body>`, keyed by the whole `whsec_...` secret. Other schemes (`v0`)
/// are ignored, as Stripe directs against downgrade attacks.
fn verify_stripe(secret: &str, request: &SignedRequest<'_>, now: i64) -> Result<(), Rejection> {
    let header = header_text(request.headers, "stripe-signature")?;
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = Some(value.trim()),
            Some(("v1", value)) => signatures.push(decode_hex(value.trim())),
            _ => {}
        }
    }
    let timestamp = timestamp.ok_or(Rejection::Malformed("Stripe-Signature: no t="))?;
    if signatures.is_empty() {
        return Err(Rejection::Malformed("Stripe-Signature: no v1="));
    }
    check_timestamp(timestamp, now)?;
    let mut signed = format!("{timestamp}.").into_bytes();
    signed.extend_from_slice(request.body);
    matches_any(secret.as_bytes(), &signed, signatures)
}

/// Standard Webhooks (Svix): `webhook-id`, `webhook-timestamp` and
/// `webhook-signature: v1,<base64>[ v1,<base64>...]` (or the `svix-`
/// spellings) sign `<id>.<timestamp>.<body>`, keyed by the base64 bytes after
/// `whsec_`. Asymmetric `v1a` signatures are not checked.
fn verify_standard(secret: &str, request: &SignedRequest<'_>, now: i64) -> Result<(), Rejection> {
    let header = |name: &str| {
        let standard = format!("webhook-{name}");
        header_text(request.headers, &standard)
            .or_else(|_| header_text(request.headers, &format!("svix-{name}")))
            .map_err(|_| Rejection::MissingHeader(standard))
    };
    let (id, timestamp, signatures) = (header("id")?, header("timestamp")?, header("signature")?);
    let key = STANDARD
        .decode(secret.strip_prefix("whsec_").unwrap_or(secret))
        .map_err(|_| Rejection::UnusableSecret("not base64 after whsec_"))?;
    if key.is_empty() {
        return Err(Rejection::UnusableSecret("empty key"));
    }
    check_timestamp(timestamp, now)?;
    let mut signed = format!("{id}.{timestamp}.").into_bytes();
    signed.extend_from_slice(request.body);
    let candidates = signatures
        .split(' ')
        .filter_map(|entry| entry.split_once(','))
        .filter(|(version, _)| *version == "v1")
        .map(|(_, value)| STANDARD.decode(value).ok());
    matches_any(&key, &signed, candidates)
}

/// A signed Unix timestamp in whole seconds, within the tolerance of `now`.
fn check_timestamp(timestamp: &str, now: i64) -> Result<(), Rejection> {
    if timestamp.is_empty() || !timestamp.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Rejection::Malformed("timestamp"));
    }
    let signed_at: i64 = timestamp
        .parse()
        .map_err(|_| Rejection::Malformed("timestamp"))?;
    let age_seconds = now.saturating_sub(signed_at);
    if age_seconds.abs() > SIGNATURE_TOLERANCE_SECONDS {
        return Err(Rejection::Stale { age_seconds });
    }
    Ok(())
}

/// Whether any candidate (undecodable ones are `None`) is the MAC of
/// `signed` under `key`.
fn matches_any(
    key: &[u8],
    signed: &[u8],
    candidates: impl IntoIterator<Item = Option<Vec<u8>>>,
) -> Result<(), Rejection> {
    let expected = mac(key, signed);
    let matched = candidates
        .into_iter()
        .flatten()
        .any(|candidate| expected.clone().verify_slice(&candidate).is_ok());
    if matched {
        Ok(())
    } else {
        Err(Rejection::Mismatch)
    }
}

fn mac(key: &[u8], data: &[u8]) -> HmacSha256 {
    // HMAC takes a key of any length, so this cannot fail.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac
}

fn header_text<'h>(headers: &'h HeaderMap, name: &str) -> Result<&'h str, Rejection> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| Rejection::MissingHeader(name.to_string()))
}

/// A MAC as providers send it: optionally `sha256=`, then 64 hex digits or
/// the base64 of its 32 bytes. The two spellings cannot be confused: 32
/// bytes are 64 hex digits but 44 base64 characters.
fn decode_mac(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    let value = value.strip_prefix("sha256=").unwrap_or(value);
    if value.len() == 64 {
        return decode_hex(value);
    }
    STANDARD.decode(value).ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
#[path = "signature_test.rs"]
mod tests;
