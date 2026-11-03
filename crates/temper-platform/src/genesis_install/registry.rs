//! The Genesis registry this process installs apps from.

use reqwest::Url;

/// The setting that names this process's Genesis registry.
const REGISTRY_SETTING: &str = "TEMPER_GENESIS_REGISTRY_URL";

/// The Genesis registry this process fetches app bundles and App rows from.
///
/// A new install takes its registry from [`configured_registry`], from
/// `TEMPER_GENESIS_REGISTRY_URL`: which host a server takes new app code from
/// is a deployment setting, so an install request may name a registry only to
/// say which one it means, and any other value is refused before a request is
/// made. Restoring an install this server already made uses
/// [`recorded_registry`], the registry that install came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryUrl(String);

impl RegistryUrl {
    /// The registry's base URL, without a trailing slash.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The configured Genesis registry, checked against the one `requested` names.
///
/// An empty `requested` means the configured registry. With no registry
/// configured, nothing is fetched.
pub(crate) fn configured_registry(requested: &str) -> Result<RegistryUrl, String> {
    let configured = std::env::var(REGISTRY_SETTING).unwrap_or_default();
    registry_for(&configured, requested)
}

/// The registry a stored install record was made from, to fetch that same
/// install's files again when its cache directory is gone.
///
/// Restoring reproduces what this server already installed, so it does not
/// depend on the current setting: a restart with the setting unset or changed
/// keeps every installed app.
pub(crate) fn recorded_registry(recorded: &str) -> Result<RegistryUrl, String> {
    comparable_form(recorded).map_err(|error| format!("recorded registry_url {error}"))?;
    Ok(RegistryUrl(
        recorded.trim().trim_end_matches('/').to_string(),
    ))
}

/// The registry `configured` names, as if it were this server's setting.
#[cfg(test)]
pub(crate) fn registry_for_test(configured: &str) -> RegistryUrl {
    registry_for(configured, "").expect("test registry URL")
}

fn registry_for(configured: &str, requested: &str) -> Result<RegistryUrl, String> {
    if configured.trim().is_empty() {
        return Err(format!(
            "no Genesis registry is configured on this server; set {REGISTRY_SETTING} to install apps from one"
        ));
    }
    let configured_form =
        comparable_form(configured).map_err(|error| format!("{REGISTRY_SETTING} {error}"))?;
    if !requested.trim().is_empty() {
        let requested_form =
            comparable_form(requested).map_err(|error| format!("registry_url {error}"))?;
        if requested_form != configured_form {
            return Err(format!(
                "registry_url must be this server's Genesis registry ({REGISTRY_SETTING}); leave it out to use that registry"
            ));
        }
    }
    Ok(RegistryUrl(
        configured.trim().trim_end_matches('/').to_string(),
    ))
}

/// A registry base URL in the form two spellings of one registry share:
/// scheme and host lower-cased, a default port dropped, no trailing slash.
fn comparable_form(raw: &str) -> Result<String, String> {
    let url = Url::parse(raw.trim()).map_err(|error| format!("is not a URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("must be an http:// or https:// URL".to_string());
    }
    // Request paths are appended to the base URL, so a query or fragment
    // would change which path is requested.
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not carry a query or fragment".to_string());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// HTTP client for registry requests.
///
/// It does not follow redirects, so every response comes from the configured
/// registry itself.
pub(crate) fn registry_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("build Genesis registry client: {error}"))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const CONFIGURED: &str = "https://genesis.example.test";

    #[test]
    fn an_empty_request_uses_the_configured_registry() {
        let registry = registry_for(" https://genesis.example.test/ ", "").expect("configured");
        assert_eq!(registry.as_str(), CONFIGURED);
    }

    #[test]
    fn another_spelling_of_the_configured_registry_is_the_same_registry() {
        for requested in [
            "https://genesis.example.test",
            "https://genesis.example.test/",
            "HTTPS://Genesis.Example.Test",
            "https://genesis.example.test:443",
        ] {
            let registry = registry_for(CONFIGURED, requested).expect(requested);
            assert_eq!(registry.as_str(), CONFIGURED, "{requested}");
        }
    }

    #[test]
    fn a_registry_other_than_the_configured_one_is_refused() {
        for requested in [
            "https://other.example.test",
            "http://genesis.example.test",
            "https://genesis.example.test:8443",
            "https://genesis.example.test/other",
            "http://127.0.0.1:18081",
            "https://genesis.example.test/?x=1",
            "https://genesis.example.test#x",
            "file:///etc",
            "not a url",
        ] {
            let error = registry_for(CONFIGURED, requested).expect_err(requested);
            assert!(error.contains("registry_url"), "{requested}: {error}");
        }
    }

    #[test]
    fn a_stored_install_is_fetched_again_from_the_registry_it_came_from() {
        let registry = recorded_registry(" http://127.0.0.1:18081/ ").expect("recorded");
        assert_eq!(registry.as_str(), "http://127.0.0.1:18081");
        for recorded in ["", "file:///etc", "https://genesis.example.test/?x=1"] {
            assert!(recorded_registry(recorded).is_err(), "{recorded}");
        }
    }

    #[test]
    fn without_a_configured_registry_nothing_is_fetched() {
        for requested in ["", CONFIGURED] {
            let error = registry_for("", requested).expect_err("unset");
            assert!(error.contains(REGISTRY_SETTING), "{error}");
        }
    }

    #[tokio::test]
    async fn the_registry_client_does_not_follow_redirects() {
        let registry = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/genesis/apps/acme/notes/versions/x/bundle"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", elsewhere.uri())),
            )
            .mount(&registry)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(0)
            .mount(&elsewhere)
            .await;

        let response = registry_client()
            .expect("client")
            .get(format!(
                "{}/api/genesis/apps/acme/notes/versions/x/bundle",
                registry.uri()
            ))
            .send()
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 302);
        elsewhere.verify().await;
    }
}
