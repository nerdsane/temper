//! A `[[webhook]]` either loads in a shape the receiver can fire, or is
//! refused when the spec loads (ADR-0182).

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};
use crate::automaton::types::{Automaton, WebhookSignature, WebhookSource};

fn spec(webhook: &str) -> String {
    format!(
        r#"
[automaton]
name = "Order"
states = ["Open", "Paid"]
initial = "Open"

[[action]]
name = "Pay"
kind = "input"
from = ["Open"]
to = "Paid"
params = ["amount_total", "event_id", "delivery"]

{webhook}
"#
    )
}

fn parse(webhook: &str) -> Result<Automaton, String> {
    parse_automaton_with_liveness(&spec(webhook), LivenessEnforcement::WarnOnly)
        .map_err(|e| e.to_string())
}

#[track_caller]
fn refused(webhook: &str, needle: &str) {
    match parse(webhook) {
        Ok(_) => panic!("loaded a webhook that should be refused with `{needle}`:\n{webhook}"),
        Err(error) => assert!(error.contains(needle), "`{needle}` not in: {error}"),
    }
}

const STRIPE: &str = r#"[[webhook]]
name = "stripe_paid"
path = "stripe/paid"
action = "Pay"
signature = "stripe"
hmac_secret = "{secret:STRIPE_WEBHOOK_SECRET}"
entity_id = "body.data.object.client_reference_id"

[webhook.extract]
amount_total = "body.data.object.amount_total"
event_id = "body.id"
delivery = "header.x-delivery"
"#;

#[test]
fn a_stripe_webhook_reads_the_body_and_headers() {
    let automaton = parse(STRIPE).expect("a signed body-reading webhook loads");
    let webhook = &automaton.webhooks[0];
    assert_eq!(webhook.signature, WebhookSignature::Stripe);
    assert_eq!(webhook.method, None, "no method declared: any method");
    assert_eq!(
        WebhookSource::parse(&webhook.entity_id),
        Some(WebhookSource::Body("data.object.client_reference_id"))
    );
    assert_eq!(
        WebhookSource::parse(&webhook.extract["delivery"]),
        Some(WebhookSource::Header("x-delivery"))
    );
}

#[test]
fn every_scheme_spelling_loads() {
    for (scheme, header) in [
        ("kernel", "hmac_header = \"x-sig\"\n"),
        ("hmac", "hmac_header = \"x-shopify-hmac-sha256\"\n"),
        ("github", ""),
        ("stripe", ""),
        ("standard_webhooks", ""),
        ("token", "hmac_header = \"x-gitlab-token\"\n"),
    ] {
        let webhook = STRIPE.replace(
            "signature = \"stripe\"\n",
            &format!("signature = \"{scheme}\"\n{header}"),
        );
        let automaton = parse(&webhook).unwrap_or_else(|e| panic!("{scheme}: {e}"));
        assert_eq!(automaton.webhooks[0].signature.as_str(), scheme);
    }
}

#[test]
fn a_webhook_without_a_secret_is_refused() {
    // It used to load and answer every request with 401.
    refused(
        &STRIPE.replace("hmac_secret = \"{secret:STRIPE_WEBHOOK_SECRET}\"\n", ""),
        "hmac_secret is required",
    );
    refused(
        &STRIPE.replace("{secret:STRIPE_WEBHOOK_SECRET}", " "),
        "hmac_secret is required",
    );
}

#[test]
fn the_signature_header_is_named_only_where_the_scheme_takes_one() {
    for scheme in ["kernel", "hmac", "token"] {
        refused(
            &STRIPE.replace("\"stripe\"", &format!("\"{scheme}\"")),
            "needs hmac_header",
        );
    }
    for scheme in ["stripe", "github", "standard_webhooks"] {
        refused(
            &STRIPE.replace(
                "signature = \"stripe\"\n",
                &format!("signature = \"{scheme}\"\nhmac_header = \"x-sig\"\n"),
            ),
            "hmac_header does not apply",
        );
    }
    refused(&STRIPE.replace("\"stripe\"", "\"paypal\""), "paypal");
}

#[test]
fn a_webhook_names_a_declared_action_and_readable_sources() {
    refused(
        &STRIPE.replace("action = \"Pay\"", "action = \"Refund\""),
        "action 'Refund' is not declared",
    );
    for source in ["form.id", "body.", "body..id", "header.", "query"] {
        refused(
            &STRIPE.replace("\"body.id\"", &format!("\"{source}\"")),
            "must be query.<name>, header.<name>, body or body.<path>",
        );
    }
    refused(
        &STRIPE.replace(
            "signature = \"stripe\"\n",
            "signature = \"stripe\"\nmethod = \"\"\n",
        ),
        "method is empty",
    );
}

#[test]
fn sources_read_their_spelling() {
    assert_eq!(WebhookSource::parse("body"), Some(WebhookSource::Body("")));
    assert_eq!(
        WebhookSource::parse("body.items.0.sku"),
        Some(WebhookSource::Body("items.0.sku"))
    );
    assert_eq!(
        WebhookSource::parse("query.state"),
        Some(WebhookSource::Query("state"))
    );
    assert_eq!(WebhookSource::parse("bodyx.id"), None);
    assert_eq!(WebhookSource::parse("state"), None);
}
