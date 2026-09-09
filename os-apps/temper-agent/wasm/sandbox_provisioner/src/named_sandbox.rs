//! Gated named-sandbox resolution for `provision_sandbox`.
//!
//! Empty name and empty URL mean "unset". Unresolved `{secret:...}` templates
//! are also unset. A name without a URL fails closed so Provision does not
//! create an ephemeral E2B sandbox when the operator asked for `dsf`.

/// True when `raw` can be used as a sandbox name or URL.
pub fn usable_value(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    if value.is_empty() || value.contains("{secret:") {
        None
    } else {
        Some(value)
    }
}

/// First usable candidate, scanning in the given order.
pub fn first_usable<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    candidates.into_iter().find_map(usable_value)
}

/// Outcome of the named-sandbox gate. Does not talk to TensorLake or E2B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedSandboxDecision {
    /// Connect the supplied URL. Id is the name when present.
    Connect {
        /// Sandbox HTTP URL.
        url: String,
        /// Sandbox id recorded on SandboxReady.
        sandbox_id: String,
    },
    /// Name was set, URL was not. Caller must not fall through to E2B.
    FailClosed {
        /// Requested sandbox name (not a secret).
        name: String,
    },
    /// Neither name nor URL is usable. Caller keeps current E2B create.
    Unset,
}

impl NamedSandboxDecision {
    /// Resolve from already-selected name and URL strings.
    pub fn from_name_and_url(name: Option<&str>, url: Option<&str>) -> Self {
        let name = usable_value(name);
        let url = usable_value(url);
        match (name, url) {
            (_, Some(url)) => Self::Connect {
                url: url.to_string(),
                sandbox_id: name.unwrap_or("named-sandbox").to_string(),
            },
            (Some(name), None) => Self::FailClosed {
                name: name.to_string(),
            },
            (None, None) => Self::Unset,
        }
    }

    /// Error text when the operator named a sandbox but gave no URL.
    pub fn fail_closed_message(name: &str) -> String {
        format!(
            "named sandbox '{name}' is set but temper_sandbox_url / TEMPER_SANDBOX_URL is empty; \
             refusing to create an ephemeral E2B sandbox. Set TEMPER_SANDBOX_URL to the TensorLake \
             sandbox URL (dsf / dd comp). This guest has no TensorLake create client."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_secret_templates_are_unusable() {
        assert_eq!(usable_value(None), None);
        assert_eq!(usable_value(Some("")), None);
        assert_eq!(usable_value(Some("   ")), None);
        assert_eq!(usable_value(Some("{secret:temper_sandbox_url}")), None);
        assert_eq!(
            usable_value(Some("https://sandbox.example")),
            Some("https://sandbox.example")
        );
        assert_eq!(usable_value(Some("dsf")), Some("dsf"));
    }

    #[test]
    fn first_usable_skips_empty_then_unresolved() {
        assert_eq!(
            first_usable([
                Some(""),
                Some("{secret:temper_sandbox_url}"),
                Some("https://dsf.example"),
            ]),
            Some("https://dsf.example")
        );
        assert_eq!(first_usable([Some(""), None]), None);
    }

    #[test]
    fn url_connects_with_name_as_id() {
        assert_eq!(
            NamedSandboxDecision::from_name_and_url(Some("dsf"), Some("https://dsf.example")),
            NamedSandboxDecision::Connect {
                url: "https://dsf.example".to_string(),
                sandbox_id: "dsf".to_string(),
            }
        );
    }

    #[test]
    fn url_without_name_uses_stable_id() {
        assert_eq!(
            NamedSandboxDecision::from_name_and_url(None, Some("https://dsf.example")),
            NamedSandboxDecision::Connect {
                url: "https://dsf.example".to_string(),
                sandbox_id: "named-sandbox".to_string(),
            }
        );
    }

    #[test]
    fn name_without_url_fails_closed() {
        assert_eq!(
            NamedSandboxDecision::from_name_and_url(Some("dsf"), None),
            NamedSandboxDecision::FailClosed {
                name: "dsf".to_string(),
            }
        );
        let message = NamedSandboxDecision::fail_closed_message("dsf");
        assert!(message.contains("dsf"), "{message}");
        assert!(message.contains("TEMPER_SANDBOX_URL"), "{message}");
        assert!(message.contains("E2B"), "{message}");
    }

    #[test]
    fn empty_name_and_url_leave_e2b_path() {
        assert_eq!(
            NamedSandboxDecision::from_name_and_url(None, None),
            NamedSandboxDecision::Unset
        );
        assert_eq!(
            NamedSandboxDecision::from_name_and_url(Some(""), Some("{secret:temper_sandbox_url}")),
            NamedSandboxDecision::Unset
        );
    }
}
