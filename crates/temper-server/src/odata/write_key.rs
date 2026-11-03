//! Every write to `/tdata` has an idempotency key, and its answer names it.
//!
//! A caller that never heard a write's answer, or heard that it was not
//! confirmed, cannot tell whether the write was applied. Sent again with the
//! key the answer named, it is applied at most once and answered as the first
//! time: the record keeps the keys of the moves it made (see
//! [`crate::idempotency`]). A move sent without a key was already given one
//! inside the kernel; it is given one here instead, where the answer can carry
//! it back.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::HeaderName;
use axum::middleware::Next;
use axum::response::Response;
use temper_runtime::scheduler::sim_uuid;

/// The header a write's key arrives in, and its answer names it in.
pub const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

/// Give a write that came without a usable key one of its own, and name the
/// write's key on its answer, whatever the answer is.
pub(crate) async fn key_every_write(mut request: Request, next: Next) -> Response {
    if request.method().is_safe() {
        return next.run(request).await;
    }
    // The handlers ignore a key that is empty or not text, so it is replaced.
    let usable = request
        .headers()
        .get(&IDEMPOTENCY_KEY)
        .filter(|key| key.to_str().is_ok_and(|key| !key.is_empty()))
        .cloned();
    let key = match usable {
        Some(key) => key,
        None => {
            let minted = HeaderValue::from_str(&sim_uuid().to_string())
                .expect("a UUID is a valid header value");
            request
                .headers_mut()
                .insert(IDEMPOTENCY_KEY, minted.clone());
            minted
        }
    };
    let mut response = next.run(request).await;
    response.headers_mut().insert(IDEMPOTENCY_KEY, key);
    response
}
