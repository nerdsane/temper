//! The local sandbox a server started, and the token its requests carry.
//!
//! `temper serve` can start a local tool sandbox for development. That
//! sandbox only answers requests that carry the token the server generated
//! when it started it. The token stays in the server: guests address the
//! sandbox by its URL alone, and the host adds the token to requests whose
//! origin is the sandbox's origin.

use std::fmt;

use super::{ProductionWasmHost, internal_http};

/// Where the server's local sandbox listens and the token it requires.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalSandboxAccess {
    /// Scheme, host and port of the sandbox, for example `http://127.0.0.1:3010`.
    origin: String,
    token: String,
}

impl LocalSandboxAccess {
    /// Describe a sandbox listening at `base_url` that requires `token`.
    pub fn new(base_url: &str, token: String) -> Result<Self, String> {
        let url = reqwest::Url::parse(base_url)
            .map_err(|err| format!("local sandbox URL {base_url} does not parse: {err}"))?;
        if url.host_str().is_none() {
            return Err(format!("local sandbox URL {base_url} names no host"));
        }
        if token.is_empty() {
            return Err("local sandbox token must not be empty".to_string());
        }
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            token,
        })
    }

    /// Whether `url` goes to the sandbox's origin (scheme, host and port).
    fn serves(&self, url: &str) -> bool {
        internal_http::is_internal_url(url, std::iter::once(self.origin.as_str()))
    }
}

impl fmt::Debug for LocalSandboxAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalSandboxAccess")
            .field("origin", &self.origin)
            .field("token", &"<held by the server>")
            .finish()
    }
}

impl ProductionWasmHost {
    /// Attach the local sandbox this server started, if any. Requests to its
    /// origin carry its token as a bearer credential.
    pub fn with_local_sandbox(mut self, sandbox: Option<LocalSandboxAccess>) -> Self {
        self.local_sandbox = sandbox;
        self
    }

    /// Whether `url` goes to the server's local sandbox.
    pub(super) fn goes_to_local_sandbox(&self, url: &str) -> bool {
        self.local_sandbox
            .as_ref()
            .is_some_and(|sandbox| sandbox.serves(url))
    }

    /// Add the sandbox token when `url` goes to the server's local sandbox.
    pub(super) fn add_local_sandbox_token(
        &self,
        builder: reqwest::RequestBuilder,
        url: &str,
    ) -> reqwest::RequestBuilder {
        match &self.local_sandbox {
            Some(sandbox) if sandbox.serves(url) => builder.bearer_auth(&sandbox.token),
            _ => builder,
        }
    }
}
