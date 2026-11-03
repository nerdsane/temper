//! Each provider scheme against the provider's published example where it
//! gives one, and against values computed independently (Python `hmac`)
//! where it does not.

use super::*;
use axum::http::{HeaderName, HeaderValue};

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    map
}

fn post<'a>(headers: &'a HeaderMap, body: &'a [u8]) -> SignedRequest<'a> {
    SignedRequest {
        method: "POST",
        path_and_query: "/webhooks/t/p?x=1",
        headers,
        body,
    }
}

fn check(
    scheme: WebhookSignature,
    secret: &str,
    header: Option<&str>,
    request: &SignedRequest<'_>,
    now: i64,
) -> Result<(), Rejection> {
    verify_signature(scheme, secret, header, request, now)
}

// GitHub publishes this vector:
// https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries#testing-the-webhook-payload-validation
const GITHUB_SECRET: &str = "It's a Secret to Everybody";
const GITHUB_SIGNATURE: &str =
    "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

#[test]
fn github_published_vector() {
    let h = headers(&[("X-Hub-Signature-256", GITHUB_SIGNATURE)]);
    let scheme = WebhookSignature::Github;
    assert_eq!(
        check(scheme, GITHUB_SECRET, None, &post(&h, b"Hello, World!"), 0),
        Ok(())
    );
    assert_eq!(
        check(scheme, GITHUB_SECRET, None, &post(&h, b"Hello, World?"), 0),
        Err(Rejection::Mismatch)
    );
    assert_eq!(
        check(
            scheme,
            "another secret",
            None,
            &post(&h, b"Hello, World!"),
            0
        ),
        Err(Rejection::Mismatch)
    );
    let none = HeaderMap::new();
    assert_eq!(
        check(
            scheme,
            GITHUB_SECRET,
            None,
            &post(&none, b"Hello, World!"),
            0
        ),
        Err(Rejection::MissingHeader("x-hub-signature-256".to_string()))
    );
}

// Svix publishes this vector ("Example signatures"):
// https://docs.svix.com/receiving/verifying-payloads/how-manual
const SVIX_SECRET: &str = "whsec_plJ3nmyCDGBKInavdOK15jsl";
const SVIX_ID: &str = "msg_loFOjxBNrRLzqYUf";
const SVIX_TIMESTAMP: i64 = 1_731_705_121;
const SVIX_BODY: &[u8] = br#"{"event_type":"ping","data":{"success":true}}"#;
const SVIX_SIGNATURE: &str = "v1,rAvfW3dJ/X/qxhsaXPOyyCGmRKsaKWcsNccKXlIktD0=";

fn standard_headers(prefix: &str, signature: &str) -> HeaderMap {
    headers(&[
        (format!("{prefix}-id").as_str(), SVIX_ID),
        (
            format!("{prefix}-timestamp").as_str(),
            SVIX_TIMESTAMP.to_string().as_str(),
        ),
        (format!("{prefix}-signature").as_str(), signature),
    ])
}

#[test]
fn standard_webhooks_published_vector() {
    let scheme = WebhookSignature::StandardWebhooks;
    for prefix in ["webhook", "svix"] {
        let h = standard_headers(prefix, SVIX_SIGNATURE);
        assert_eq!(
            check(
                scheme,
                SVIX_SECRET,
                None,
                &post(&h, SVIX_BODY),
                SVIX_TIMESTAMP
            ),
            Ok(()),
            "{prefix}-* headers"
        );
    }
    // A rotated secret sends several signatures; one match admits.
    let rotated = standard_headers(
        "webhook",
        format!("v1,AAAAbm90IHRoZSBvbmU= v1a,ignored {SVIX_SIGNATURE}").as_str(),
    );
    assert_eq!(
        check(
            scheme,
            SVIX_SECRET,
            None,
            &post(&rotated, SVIX_BODY),
            SVIX_TIMESTAMP
        ),
        Ok(())
    );
    let h = standard_headers("webhook", SVIX_SIGNATURE);
    assert_eq!(
        check(scheme, SVIX_SECRET, None, &post(&h, b"{}"), SVIX_TIMESTAMP),
        Err(Rejection::Mismatch)
    );
    let asymmetric_only = standard_headers("webhook", "v1a,cmF3");
    assert_eq!(
        check(
            scheme,
            SVIX_SECRET,
            None,
            &post(&asymmetric_only, SVIX_BODY),
            SVIX_TIMESTAMP
        ),
        Err(Rejection::Mismatch)
    );
    assert_eq!(
        check(
            scheme,
            "whsec_not*base64",
            None,
            &post(&h, SVIX_BODY),
            SVIX_TIMESTAMP
        ),
        Err(Rejection::UnusableSecret("not base64 after whsec_"))
    );
}

#[test]
fn signed_timestamps_refuse_replay() {
    let scheme = WebhookSignature::StandardWebhooks;
    let h = standard_headers("webhook", SVIX_SIGNATURE);
    let request = post(&h, SVIX_BODY);
    let edge = SVIX_TIMESTAMP + SIGNATURE_TOLERANCE_SECONDS;
    assert_eq!(check(scheme, SVIX_SECRET, None, &request, edge), Ok(()));
    assert_eq!(
        check(scheme, SVIX_SECRET, None, &request, edge + 1),
        Err(Rejection::Stale {
            age_seconds: SIGNATURE_TOLERANCE_SECONDS + 1
        })
    );
    assert_eq!(
        check(
            scheme,
            SVIX_SECRET,
            None,
            &request,
            SVIX_TIMESTAMP - SIGNATURE_TOLERANCE_SECONDS - 1
        ),
        Err(Rejection::Stale {
            age_seconds: -(SIGNATURE_TOLERANCE_SECONDS + 1)
        })
    );
}

// Stripe documents the format (https://docs.stripe.com/webhooks#verify-manually)
// but publishes no secret for its example header, so the expected value is
// HMAC-SHA256(key = "whsec_test_secret", "1492774577." + body), computed
// with Python's hmac module.
const STRIPE_SECRET: &str = "whsec_test_secret";
const STRIPE_T: i64 = 1_492_774_577;
const STRIPE_BODY: &[u8] = br#"{"id":"evt_test_webhook","object":"event"}"#;
const STRIPE_V1: &str = "88a022085c6bdb887b02cb26ff76dd681234d9675c0f22844059f55552a8883a";

#[test]
fn stripe_signature() {
    let scheme = WebhookSignature::Stripe;
    let signed = headers(&[(
        "Stripe-Signature",
        format!("t={STRIPE_T},v1={STRIPE_V1},v0=6ffbb59b2300aae63f272406069a9788598b792a944a07aba816edb039989a39").as_str(),
    )]);
    assert_eq!(
        check(
            scheme,
            STRIPE_SECRET,
            None,
            &post(&signed, STRIPE_BODY),
            STRIPE_T
        ),
        Ok(())
    );
    assert_eq!(
        check(scheme, STRIPE_SECRET, None, &post(&signed, b"{}"), STRIPE_T),
        Err(Rejection::Mismatch)
    );
    assert_eq!(
        check(
            scheme,
            STRIPE_SECRET,
            None,
            &post(&signed, STRIPE_BODY),
            STRIPE_T + 301
        ),
        Err(Rejection::Stale { age_seconds: 301 })
    );
    // Only v1 counts: a v0 signature alone never admits (downgrade).
    let v0_only = headers(&[(
        "Stripe-Signature",
        format!("t={STRIPE_T},v0={STRIPE_V1}").as_str(),
    )]);
    assert_eq!(
        check(
            scheme,
            STRIPE_SECRET,
            None,
            &post(&v0_only, STRIPE_BODY),
            STRIPE_T
        ),
        Err(Rejection::Malformed("Stripe-Signature: no v1="))
    );
    // The signed time is the one in the header: changing it breaks the MAC.
    let moved = headers(&[(
        "Stripe-Signature",
        format!("t={},v1={STRIPE_V1}", STRIPE_T + 1).as_str(),
    )]);
    assert_eq!(
        check(
            scheme,
            STRIPE_SECRET,
            None,
            &post(&moved, STRIPE_BODY),
            STRIPE_T
        ),
        Err(Rejection::Mismatch)
    );
}

// HMAC-SHA256(key = "shpss_test", body), computed with Python's hmac module.
const HMAC_BODY: &[u8] = br#"{"id":820982911946154508}"#;
const HMAC_HEX: &str = "dc9c573bc2ee979da20f9faa6f5147b685caf2f0f03db5e98bc781fb21139392";
const HMAC_BASE64: &str = "3JxXO8Lul52iD5+qb1FHtoXK8vDwPbXpi8eB+yETk5I=";

#[test]
fn generic_body_hmac_in_hex_or_base64() {
    let scheme = WebhookSignature::Hmac;
    let name = Some("x-shopify-hmac-sha256");
    let prefixed = format!("sha256={HMAC_HEX}");
    for value in [HMAC_HEX, HMAC_BASE64, prefixed.as_str()] {
        let h = headers(&[("X-Shopify-Hmac-Sha256", value)]);
        assert_eq!(
            check(scheme, "shpss_test", name, &post(&h, HMAC_BODY), 0),
            Ok(()),
            "{value}"
        );
    }
    let h = headers(&[("X-Shopify-Hmac-Sha256", &HMAC_HEX[..62])]);
    assert_eq!(
        check(scheme, "shpss_test", name, &post(&h, HMAC_BODY), 0),
        Err(Rejection::Mismatch)
    );
}

// HMAC-SHA256(key = "kernel-secret", "POST\n/webhooks/t/p?x=1\n{\"a\":1}"),
// computed with Python's hmac module: the scheme before ADR-0182, unchanged.
#[test]
fn kernel_scheme_signs_method_path_and_body() {
    let scheme = WebhookSignature::Kernel;
    let sig = "b401150004071493727748c0e539982a6bdb009ee0e18caee8c53439c638795e";
    let h = headers(&[("x-webhook-signature", sig)]);
    let name = Some("x-webhook-signature");
    assert_eq!(
        check(scheme, "kernel-secret", name, &post(&h, br#"{"a":1}"#), 0),
        Ok(())
    );
    let get = SignedRequest {
        method: "GET",
        ..post(&h, br#"{"a":1}"#)
    };
    assert_eq!(
        check(scheme, "kernel-secret", name, &get, 0),
        Err(Rejection::Mismatch)
    );
}

#[test]
fn token_scheme_compares_the_secret() {
    let scheme = WebhookSignature::Token;
    let name = Some("x-gitlab-token");
    let h = headers(&[("X-Gitlab-Token", "s3cret")]);
    assert_eq!(check(scheme, "s3cret", name, &post(&h, b"{}"), 0), Ok(()));
    assert_eq!(
        check(scheme, "s3cret!", name, &post(&h, b"{}"), 0),
        Err(Rejection::Mismatch)
    );
    assert_eq!(
        check(scheme, "", name, &post(&h, b"{}"), 0),
        Err(Rejection::UnusableSecret("empty"))
    );
}
