//! ES256 (ECDSA P-256) JWT verification for platform-issued access tokens.
//!
//! The platform authorization server (and, during the ARN-255 rollout,
//! katagami.ai's authorization server as the first allowlisted issuer) mints
//! short-lived ES256 JWTs. This module verifies one against a registered
//! issuer's JWKS and returns the validated claims. See RFC-0002.
//!
//! Design constraints:
//! - **ES256 only.** The header `alg` must be exactly `ES256`; every other
//!   value — including `none`, `HS256`, and `RS256` — is rejected before any
//!   key is consulted. The verification path only ever performs P-256 ECDSA,
//!   so an algorithm-substitution ("alg confusion") attack cannot select a
//!   different primitive.
//! - **Time is caller-supplied.** `exp`/`nbf` are validated against a
//!   `now_unix` the caller derives from `sim_now()`, never a wall clock, so
//!   verification is deterministic under DST. No JWT library's internal clock
//!   is involved.
//! - **The signature is the gate.** The unverified `iss` claim is only used by
//!   the caller to select which registered issuer's keys to check against; a
//!   forged token fails the signature check because it lacks the issuer's
//!   private key.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde::Deserialize;

/// The only JWS algorithm this verifier accepts.
const REQUIRED_ALG: &str = "ES256";

/// Reasons a token is rejected. Kept coarse on purpose: callers log the
/// variant internally but must not leak which step failed back to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JwtError {
    /// Not three non-empty base64url segments, or a segment failed to decode.
    Malformed,
    /// Header `alg` was not exactly `ES256` (covers `none`, HS*, RS*, PS*, …).
    UnsupportedAlg,
    /// No JWK in the issuer's set matched the token's `kid`.
    UnknownKid,
    /// A matched JWK could not be turned into a P-256 verifying key.
    BadKey,
    /// The signature did not verify against the selected key.
    BadSignature,
    /// `exp` is missing, or `now > exp + leeway`.
    Expired,
    /// `nbf` is present and `now < nbf - leeway`.
    NotYetValid,
    /// `iss` did not equal the expected issuer.
    WrongIssuer,
    /// `aud` did not contain the expected audience.
    WrongAudience,
}

/// A single JSON Web Key (the subset this verifier needs).
#[derive(Debug, Clone, Deserialize)]
pub struct Jwk {
    /// Key type; must be `EC`.
    pub kty: String,
    /// Curve; must be `P-256`.
    #[serde(default)]
    pub crv: String,
    /// Key ID, matched against the token header's `kid`.
    #[serde(default)]
    pub kid: Option<String>,
    /// Base64url X coordinate (32 bytes once decoded).
    #[serde(default)]
    pub x: String,
    /// Base64url Y coordinate (32 bytes once decoded).
    #[serde(default)]
    pub y: String,
}

/// A JWKS document — a set of JWKs.
#[derive(Debug, Clone, Deserialize)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// JWT header (the fields this verifier reads).
#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// `aud` may be a single string or an array of strings per RFC 7519.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Claims {
    /// The `exp` claim (seconds since epoch). Used by the resolver to cap how
    /// long a verified token may be cached — never past its own expiry.
    pub fn expiry(&self) -> i64 {
        self.exp
    }
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Audience::One(a) => a == expected,
            Audience::Many(list) => list.iter().any(|a| a == expected),
        }
    }
}

/// The validated claim set returned on success.
///
/// Fields beyond the registered/standard set are ignored. `sub` is the owning
/// human; `client_id` is the acting agent; `agent_type` drives Cedar; the
/// remaining optional fields are carried for downstream use (grant liveness,
/// sign-out-everywhere) without being validated here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Claims {
    pub iss: String,
    #[serde(default)]
    pub sub: Option<String>,
    aud: Audience,
    exp: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    /// The owning human's role (owner/curator/contributor), set by the AS from
    /// the Member record. Carried onto the principal for Cedar evaluation.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub grant_id: Option<String>,
    #[serde(default)]
    pub auth_generation: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Decode the claim segment WITHOUT verifying the signature.
///
/// The only legitimate use is reading `iss` to select which registered issuer
/// to verify against; the returned claims are untrusted until [`verify`] has
/// run against that issuer's keys.
pub fn decode_claims_unverified(token: &str) -> Result<Claims, JwtError> {
    let mut parts = token.split('.');
    let (_h, payload, _s) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) if !h.is_empty() && !p.is_empty() && !s.is_empty() => {
            (h, p, s)
        }
        _ => return Err(JwtError::Malformed),
    };
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| JwtError::Malformed)?;
    serde_json::from_slice(&payload_bytes).map_err(|_| JwtError::Malformed)
}

/// Verify an ES256 JWT against a registered issuer's key set and validate its
/// standard claims.
///
/// - `now_unix` MUST come from `sim_now().timestamp()`.
/// - `leeway_secs` absorbs small clock skew between issuer and kernel in
///   production; under DST it is deterministic like everything else.
///
/// On success the signature verified, `alg == ES256`, `iss`/`aud` matched, and
/// the token is within its `nbf`/`exp` window.
pub fn verify(
    token: &str,
    jwks: &Jwks,
    expected_iss: &str,
    expected_aud: &str,
    now_unix: i64,
    leeway_secs: i64,
) -> Result<Claims, JwtError> {
    // 1. Split into exactly three non-empty segments.
    let mut parts = token.split('.');
    let (header_b64, payload_b64, sig_b64) =
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None)
                if !h.is_empty() && !p.is_empty() && !s.is_empty() =>
            {
                (h, p, s)
            }
            _ => return Err(JwtError::Malformed),
        };

    // 2. Header: require alg == ES256 before touching any key material.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|_| JwtError::Malformed)?;
    let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| JwtError::Malformed)?;
    if header.alg != REQUIRED_ALG {
        return Err(JwtError::UnsupportedAlg);
    }

    // 3. Select the key by kid. If the token names a kid, it must match; if it
    //    omits one, a single-key set is unambiguous, otherwise reject.
    let jwk = select_key(jwks, header.kid.as_deref())?;
    let verifying_key = verifying_key_from_jwk(jwk)?;

    // 4. Verify the signature over "header.payload" (raw r||s, 64 bytes).
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| JwtError::Malformed)?;
    let signature = Signature::from_slice(&sig_bytes).map_err(|_| JwtError::BadSignature)?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    verifying_key
        .verify(signing_input.as_bytes(), &signature)
        .map_err(|_| JwtError::BadSignature)?;

    // 5. Signature is valid — now parse and validate claims.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| JwtError::Malformed)?;
    let claims: Claims = serde_json::from_slice(&payload_bytes).map_err(|_| JwtError::Malformed)?;

    if claims.iss != expected_iss {
        return Err(JwtError::WrongIssuer);
    }
    if !claims.aud.contains(expected_aud) {
        return Err(JwtError::WrongAudience);
    }
    if now_unix > claims.exp + leeway_secs {
        return Err(JwtError::Expired);
    }
    if let Some(nbf) = claims.nbf
        && now_unix < nbf - leeway_secs
    {
        return Err(JwtError::NotYetValid);
    }

    Ok(claims)
}

/// Pick the JWK to verify against, honoring the token's `kid`.
fn select_key<'a>(jwks: &'a Jwks, kid: Option<&str>) -> Result<&'a Jwk, JwtError> {
    match kid {
        Some(k) => jwks
            .keys
            .iter()
            .find(|j| j.kid.as_deref() == Some(k))
            .ok_or(JwtError::UnknownKid),
        // No kid in the header: only unambiguous if the set has exactly one key.
        None => match jwks.keys.as_slice() {
            [single] => Ok(single),
            _ => Err(JwtError::UnknownKid),
        },
    }
}

/// Build a P-256 verifying key from a JWK's affine coordinates.
fn verifying_key_from_jwk(jwk: &Jwk) -> Result<VerifyingKey, JwtError> {
    if jwk.kty != "EC" || jwk.crv != "P-256" {
        return Err(JwtError::BadKey);
    }
    let x = URL_SAFE_NO_PAD
        .decode(&jwk.x)
        .map_err(|_| JwtError::BadKey)?;
    let y = URL_SAFE_NO_PAD
        .decode(&jwk.y)
        .map_err(|_| JwtError::BadKey)?;
    if x.len() != 32 || y.len() != 32 {
        return Err(JwtError::BadKey);
    }
    let point = p256::EncodedPoint::from_affine_coordinates(
        p256::FieldBytes::from_slice(&x),
        p256::FieldBytes::from_slice(&y),
        false,
    );
    let key = VerifyingKey::from_encoded_point(&point).map_err(|_| JwtError::BadKey)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature as EcdsaSig, SigningKey};

    // A fixed, deterministic test keypair (DST-safe — no randomness at test time).
    fn test_key() -> SigningKey {
        // 32-byte scalar; fixed so tests are reproducible.
        let bytes = [7u8; 32];
        SigningKey::from_slice(&bytes).expect("valid scalar")
    }

    fn jwks_for(sk: &SigningKey, kid: &str) -> Jwks {
        let vk = sk.verifying_key();
        let point = vk.to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(point.x().unwrap());
        let y = URL_SAFE_NO_PAD.encode(point.y().unwrap());
        Jwks {
            keys: vec![Jwk {
                kty: "EC".into(),
                crv: "P-256".into(),
                kid: Some(kid.into()),
                x,
                y,
            }],
        }
    }

    fn b64(v: &serde_json::Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
    }

    /// Mint a signed ES256 token from header+claims JSON.
    fn mint(sk: &SigningKey, header: serde_json::Value, claims: serde_json::Value) -> String {
        let signing_input = format!("{}.{}", b64(&header), b64(&claims));
        let sig: EcdsaSig = sk.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("{signing_input}.{sig_b64}")
    }

    fn valid_header() -> serde_json::Value {
        serde_json::json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" })
    }

    fn valid_claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "https://katagami.ai",
            "sub": "human-sub-123",
            "aud": "temper",
            "exp": 2000,
            "nbf": 1000,
            "client_id": "agent-xyz",
            "agent_type": "contributor",
        })
    }

    fn verify_valid(token: &str, jwks: &Jwks) -> Result<Claims, JwtError> {
        verify(token, jwks, "https://katagami.ai", "temper", 1500, 60)
    }

    #[test]
    fn valid_token_verifies_and_maps_claims() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        let claims = verify_valid(&token, &jwks).expect("should verify");
        assert_eq!(claims.iss, "https://katagami.ai");
        assert_eq!(claims.sub.as_deref(), Some("human-sub-123"));
        assert_eq!(claims.client_id.as_deref(), Some("agent-xyz"));
        assert_eq!(claims.agent_type.as_deref(), Some("contributor"));
    }

    #[test]
    fn alg_none_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        // alg=none, empty signature — the classic bypass.
        let header = serde_json::json!({ "alg": "none", "kid": "k1" });
        let signing_input = format!("{}.{}", b64(&header), b64(&valid_claims()));
        let token = format!("{signing_input}.");
        // Empty third segment → Malformed before alg is even read.
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::Malformed));
    }

    #[test]
    fn alg_none_with_nonempty_sig_is_unsupported() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let header = serde_json::json!({ "alg": "none", "kid": "k1" });
        let token = mint(&sk, header, valid_claims());
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::UnsupportedAlg));
    }

    #[test]
    fn alg_confusion_hs256_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let header = serde_json::json!({ "alg": "HS256", "kid": "k1" });
        let token = mint(&sk, header, valid_claims());
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::UnsupportedAlg));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let sk = test_key();
        // Verify against a different key than the one that signed.
        let other = SigningKey::from_slice(&[9u8; 32]).unwrap();
        let jwks = jwks_for(&other, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::BadSignature));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        // Swap the payload for one granting a different agent_type, keep the sig.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = b64(&serde_json::json!({
            "iss": "https://katagami.ai", "aud": "temper", "exp": 2000,
            "agent_type": "owner",
        }));
        parts[1] = &forged;
        let token = parts.join(".");
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::BadSignature));
    }

    #[test]
    fn expired_token_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        // now = 3000, exp = 2000, leeway 60 → expired.
        let r = verify(&token, &jwks, "https://katagami.ai", "temper", 3000, 60);
        assert_eq!(r, Err(JwtError::Expired));
    }

    #[test]
    fn not_yet_valid_token_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        // now = 500, nbf = 1000, leeway 60 → not yet valid.
        let r = verify(&token, &jwks, "https://katagami.ai", "temper", 500, 60);
        assert_eq!(r, Err(JwtError::NotYetValid));
    }

    #[test]
    fn leeway_admits_small_skew() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        // now = 2030, exp = 2000, leeway 60 → still valid within skew.
        assert!(verify(&token, &jwks, "https://katagami.ai", "temper", 2030, 60).is_ok());
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        let r = verify(&token, &jwks, "https://evil.example", "temper", 1500, 60);
        assert_eq!(r, Err(JwtError::WrongIssuer));
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let token = mint(&sk, valid_header(), valid_claims());
        let r = verify(&token, &jwks, "https://katagami.ai", "other-app", 1500, 60);
        assert_eq!(r, Err(JwtError::WrongAudience));
    }

    #[test]
    fn audience_array_is_honored() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let mut claims = valid_claims();
        claims["aud"] = serde_json::json!(["other", "temper"]);
        let token = mint(&sk, valid_header(), claims);
        assert!(verify_valid(&token, &jwks).is_ok());
    }

    #[test]
    fn unknown_kid_is_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        let header = serde_json::json!({ "alg": "ES256", "kid": "does-not-exist" });
        let token = mint(&sk, header, valid_claims());
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::UnknownKid));
    }

    #[test]
    fn missing_kid_with_multiple_keys_is_rejected() {
        let sk = test_key();
        let mut jwks = jwks_for(&sk, "k1");
        // Add a second key so a kid-less header is ambiguous.
        let mut second = jwks.keys[0].clone();
        second.kid = Some("k2".into());
        jwks.keys.push(second);
        let header = serde_json::json!({ "alg": "ES256" });
        let token = mint(&sk, header, valid_claims());
        assert_eq!(verify_valid(&token, &jwks), Err(JwtError::UnknownKid));
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let sk = test_key();
        let jwks = jwks_for(&sk, "k1");
        for bad in ["", "a.b", "a.b.c.d", "..", "a..c", ".b.c"] {
            assert_eq!(
                verify_valid(bad, &jwks),
                Err(JwtError::Malformed),
                "token {bad:?} should be malformed"
            );
        }
    }

    #[test]
    fn decode_unverified_reads_iss_without_a_key() {
        let sk = test_key();
        let token = mint(&sk, valid_header(), valid_claims());
        let claims = decode_claims_unverified(&token).expect("decodes");
        assert_eq!(claims.iss, "https://katagami.ai");
    }
}
