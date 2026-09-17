//! Human consent contract for connector identity setup.
//!
//! This module does not perform administration. The caller must obtain the
//! response from ClientRequester, never from tool arguments or execute code.

use serde_json::{Value, json};

/// A positive native setup response, distinct from ordinary decision approval.
pub(crate) struct SetupConsent(());

impl SetupConsent {
    /// Accept only the exact setup choice in an affirmative MCP response.
    pub(crate) fn from_client_response(response: &Value) -> Option<Self> {
        if response.get("error").is_some() || response.get("method").is_some() {
            return None;
        }
        let result = response.get("result")?;
        if result.get("action")?.as_str()? != "accept" {
            return None;
        }
        let content = result.get("content")?.as_object()?;
        if content.len() != 1 || content.get("setup")?.as_str()? != "configure_agent_identity" {
            return None;
        }
        Some(Self(()))
    }
}

/// Build the native card from trusted connector configuration only.
pub(crate) fn setup_params(
    server: &str,
    tenant: &str,
    principal: &str,
    path: &std::path::Path,
) -> Value {
    let path = path.display();
    json!({
        "message": format!(
            "Set up separate agent and human identities for this Temper connection?\n\n\
             Server: {server}\nTenant: {tenant}\nRequester: {principal}\nPrivate identity file: {path}\n\n\
             This grants the verified operator permission to manage agent identity \
             records in this tenant and creates a separate nonoperator requester. \
             The requester credential will be saved in the connector's configured \
             private identity file. The operator credential remains the human \
             approval credential. This does not approve any pending agent action."
        ),
        "requestedSchema": {
            "type": "object",
            "properties": {
                "setup": {
                    "type": "string",
                    "title": "Connection setup",
                    "enum": ["configure_agent_identity", "leave_unchanged"],
                    "enumNames": ["Set up these identities", "Leave unchanged"]
                }
            },
            "required": ["setup"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_native_setup_acceptance_authorizes_setup() {
        let accepted = json!({"result": {"action": "accept", "content": {
            "setup": "configure_agent_identity"
        }}});
        assert!(SetupConsent::from_client_response(&accepted).is_some());
        for invalid in [
            json!({"setup": "configure_agent_identity"}),
            json!({"result": {"action": "decline", "content": {
                "setup": "configure_agent_identity"
            }}}),
            json!({"result": {"action": "cancel"}}),
            json!({"result": {"action": "accept", "content": {
                "setup": "leave_unchanged"
            }}}),
            json!({"result": {"action": "accept", "content": {
                "decision": "approve_broad"
            }}}),
            json!({"result": {"action": "accept", "content": {
                "setup": "configure_agent_identity", "policy": "arbitrary"
            }}}),
            json!({"error": {"code": -1}}),
        ] {
            assert!(SetupConsent::from_client_response(&invalid).is_none());
        }
    }

    #[test]
    fn card_discloses_target_and_persistent_operator_authority() {
        let params = setup_params(
            "https://genesis.example",
            "default",
            "mcp-example",
            std::path::Path::new("/private/requester.json"),
        );
        let message = params["message"].as_str().unwrap();
        assert!(message.contains("https://genesis.example"));
        assert!(message.contains("Tenant: default"));
        assert!(message.contains("Requester: mcp-example"));
        assert!(message.contains("/private/requester.json"));
        assert!(message.contains("manage agent identity records"));
        assert!(message.contains("does not approve any pending agent action"));
    }
}
