//! Redirects for WASM egress, followed one gated hop at a time.
//!
//! The egress clients follow no redirects themselves (`http_clients.rs`),
//! because the `http_call` gate sees only the URL the guest names. The host
//! follows them here instead and asks the same gate about every hop before it
//! sends it, so a 3xx can only lead to a host the guest could call directly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header;
use reqwest::{Method, StatusCode, Url};

use crate::authorized_host::{WasmAuthzGate, authorize_outbound};
use crate::types::WasmAuthzContext;

use super::{ProductionWasmHost, internal_http};

impl ProductionWasmHost {
    /// Follow redirects, checking each hop with the gate and context the
    /// wrapping `AuthorizedWasmHost` uses. Pass that same pair.
    pub fn with_redirect_gate(
        mut self,
        gate: Arc<dyn WasmAuthzGate>,
        ctx: WasmAuthzContext,
    ) -> Self {
        self.redirect_gate = Some((gate, ctx));
        self
    }

    pub(super) fn redirects(&self) -> Redirects {
        Redirects {
            gate: self.redirect_gate.clone(),
            internal_api_base_url: self.internal_api_base_url.clone(),
            timeout: self.http_timeout,
        }
    }
}

/// Most redirects one call follows. A server can answer with redirects
/// forever, and without a bound one guest call would never end. Ten is
/// reqwest's default, which guest calls followed before each hop went through
/// the gate.
const REDIRECT_HOP_BUDGET: usize = 10;

/// Why an outbound call ended without a response.
pub(super) enum OutboundError {
    /// The transport failed.
    Http(reqwest::Error),
    /// A redirect hop was refused: the gate denied it or the budget ran out.
    Refused(String),
}

impl OutboundError {
    /// reqwest's error names the whole URL, and this one reaches logs, spans and the module: a URL can hold a key.
    fn transport(error: reqwest::Error) -> Self {
        Self::Http(error.without_url())
    }

    /// The message the guest sees: a refusal as it stands, a transport error
    /// as the calling path words it.
    pub(super) fn into_message(self, transport: impl FnOnce(reqwest::Error) -> String) -> String {
        match self {
            Self::Http(error) => transport(error),
            Self::Refused(reason) => reason,
        }
    }
}

impl std::fmt::Display for OutboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => error.fmt(f),
            Self::Refused(reason) => f.write_str(reason),
        }
    }
}

/// What following a redirect needs from the host. Cloned into the task that
/// sends a streaming request.
#[derive(Clone)]
pub(super) struct Redirects {
    /// The gate and context the host's `AuthorizedWasmHost` uses. Without
    /// them no redirect is followed: the 3xx goes back to the guest.
    pub(super) gate: Option<(Arc<dyn WasmAuthzGate>, WasmAuthzContext)>,
    /// The internal Temper API, which redirects neither leave nor enter: its
    /// capabilities are bound to the one request the guest made.
    pub(super) internal_api_base_url: Option<String>,
    /// The clients' request timeout, shared by the whole chain.
    pub(super) timeout: Duration,
}

impl Redirects {
    /// Send the request `builder` holds and follow its redirects, each hop
    /// checked by the gate before it is sent.
    pub(super) async fn send(
        &self,
        client: &reqwest::Client,
        builder: reqwest::RequestBuilder,
        operation: &str,
    ) -> Result<reqwest::Response, OutboundError> {
        let request = builder.build().map_err(OutboundError::transport)?;
        let (gate, ctx) = match &self.gate {
            Some((gate, ctx)) if !self.is_internal(request.url()) => (gate, ctx),
            _ => {
                return client
                    .execute(request)
                    .await
                    .map_err(OutboundError::transport);
            }
        };
        // The chain shares one timeout, as it did when reqwest followed it. A
        // timeout past the clock's range has no deadline: each hop keeps the
        // client's own, which waits as long.
        let deadline = Instant::now().checked_add(self.timeout);
        let mut hop = Hop::of(&request);
        let mut response = client
            .execute(request)
            .await
            .map_err(OutboundError::transport)?;
        let mut budget = REDIRECT_HOP_BUDGET;
        while let Some(next) = hop.next(&response) {
            if self.is_internal(next.request.url()) {
                break;
            }
            if budget == 0 {
                return Err(OutboundError::Refused(format!(
                    "{operation} refused: more than {REDIRECT_HOP_BUDGET} redirects"
                )));
            }
            budget -= 1;
            authorize_outbound(
                gate.as_ref(),
                ctx,
                &format!("{operation} redirect"),
                next.request.method().as_str(),
                next.request.url().as_str(),
            )
            .map_err(OutboundError::Refused)?;
            response = client
                .execute(next.request(deadline))
                .await
                .map_err(OutboundError::transport)?;
            hop = next;
        }
        Ok(response)
    }

    fn is_internal(&self, url: &Url) -> bool {
        internal_http::is_internal_url(
            url.as_str(),
            self.internal_api_base_url.iter().map(String::as_str),
        )
    }
}

/// One request of a redirect chain, kept so the next hop can be built from it.
struct Hop {
    /// A copy of the request, sharing its body bytes. A streamed body cannot
    /// be sent again, so the copy of a streamed request has none.
    request: reqwest::Request,
    /// The request's body was a stream.
    streamed: bool,
}

impl Hop {
    fn of(request: &reqwest::Request) -> Self {
        if let Some(copy) = request.try_clone() {
            return Self {
                request: copy,
                streamed: false,
            };
        }
        let mut copy = reqwest::Request::new(request.method().clone(), request.url().clone());
        *copy.headers_mut() = request.headers().clone();
        Self {
            request: copy,
            streamed: true,
        }
    }

    /// The request a redirect `response` asks for, by the rules reqwest's own
    /// policy (tower-http's `FollowRedirect`) used; `None` when it is not one
    /// to follow.
    fn next(&self, response: &reqwest::Response) -> Option<Self> {
        // 301 and 302 turn a POST into a GET, and 303 everything but a HEAD;
        // either way the body is dropped. Any other request is resent as it
        // was, which a streamed body cannot be.
        let previous = self.request.url();
        let to_get = match response.status() {
            StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => {
                self.request.method() == Method::POST
            }
            StatusCode::SEE_OTHER => true,
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT => false,
            _ => return None,
        };
        if self.streamed && !to_get {
            return None;
        }
        let location = response.headers().get(header::LOCATION)?.to_str().ok()?;
        let mut next = self.copy();
        *next.url_mut() = previous.join(location).ok()?;
        if to_get {
            if next.method() != Method::HEAD {
                *next.method_mut() = Method::GET;
            }
            *next.body_mut() = None;
            for name in [
                header::CONTENT_TYPE,
                header::CONTENT_LENGTH,
                header::CONTENT_ENCODING,
                header::TRANSFER_ENCODING,
            ] {
                next.headers_mut().remove(name);
            }
        }
        if next.url().host_str() != previous.host_str()
            || next.url().port_or_known_default() != previous.port_or_known_default()
        {
            for name in [
                header::AUTHORIZATION,
                header::COOKIE,
                header::PROXY_AUTHORIZATION,
                header::WWW_AUTHENTICATE,
            ] {
                next.headers_mut().remove(name);
            }
            next.headers_mut().remove("cookie2");
        }
        Some(Self {
            request: next,
            streamed: false,
        })
    }

    /// The hop's request, with what is left of the chain's `deadline`.
    fn request(&self, deadline: Option<Instant>) -> reqwest::Request {
        let mut request = self.copy();
        if let Some(deadline) = deadline {
            *request.timeout_mut() = Some(deadline.saturating_duration_since(Instant::now()));
        }
        request
    }

    fn copy(&self) -> reqwest::Request {
        self.request
            .try_clone()
            .expect("a kept request has no streamed body")
    }
}
