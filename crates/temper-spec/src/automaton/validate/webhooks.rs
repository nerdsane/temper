//! `[[webhook]]` checks (ADR-0182): a declaration either can fire or is
//! refused here, when the spec loads, with what to write instead.

use super::super::parser::AutomatonParseError;
use super::super::types::{Automaton, Webhook, WebhookSignature, WebhookSource};

/// Each webhook names a declared action, a secret, its signature header when
/// its scheme takes one, and sources the receiver can read.
pub(super) fn validate_webhooks(
    automaton: &Automaton,
    action_names: &[&str],
) -> Result<(), AutomatonParseError> {
    for webhook in &automaton.webhooks {
        validate_webhook(webhook, action_names).map_err(|reason| {
            AutomatonParseError::Validation(format!("webhook '{}': {reason}", webhook.name))
        })?;
    }
    Ok(())
}

fn validate_webhook(webhook: &Webhook, action_names: &[&str]) -> Result<(), String> {
    if !action_names.contains(&webhook.action.as_str()) {
        return Err(format!(
            "action '{}' is not declared on this entity",
            webhook.action
        ));
    }
    if webhook
        .hmac_secret
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
    {
        // An unsigned endpoint would let anyone forge the provider's events
        // (a fake "payment succeeded"), so the receiver admits only signed
        // requests; without a secret this webhook could never fire.
        return Err("hmac_secret is required: the receiver admits only requests signed with the sender's secret. \
             Name it, such as hmac_secret = \"{secret:STRIPE_WEBHOOK_SECRET}\""
            .to_string());
    }
    let scheme = webhook.signature;
    let header = webhook
        .hmac_header
        .as_deref()
        .filter(|h| !h.trim().is_empty());
    match (scheme.names_its_header(), header) {
        (true, None) => {
            let carried = if scheme == WebhookSignature::Token {
                "secret"
            } else {
                "signature"
            };
            return Err(format!(
                "signature = \"{}\" needs hmac_header, the header the {carried} arrives in",
                scheme.as_str()
            ));
        }
        (false, Some(_)) => {
            return Err(format!(
                "hmac_header does not apply to signature = \"{}\", whose provider fixes the header; \
                 use signature = \"hmac\" to name another header",
                scheme.as_str()
            ));
        }
        _ => {}
    }
    if webhook
        .method
        .as_deref()
        .is_some_and(|m| m.trim().is_empty())
    {
        return Err(
            "method is empty: name one HTTP method, or leave method out to answer any".to_string(),
        );
    }
    let sources = std::iter::once(("entity_id", &webhook.entity_id))
        .chain(webhook.extract.iter().map(|(k, v)| (k.as_str(), v)));
    for (key, source) in sources {
        if WebhookSource::parse(source).is_none() {
            return Err(format!(
                "{key} = \"{source}\" must be query.<name>, header.<name>, body or body.<path>"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "webhooks_test.rs"]
mod tests;
