//! Embedded local Observe entry and cookie-authenticated API adapter.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use base64::Engine as _;
use rand::RngCore as _;
use serde::Deserialize;

const LOCAL_OBSERVE_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Temper Observe</title><style>
:root{color-scheme:dark}body{font:15px ui-monospace,SFMono-Regular,monospace;background:#111318;color:#e8ecf3;margin:0}
main{max-width:1000px;margin:4rem auto;padding:0 2rem}h1{font:600 2rem system-ui;margin-bottom:.25rem}
.muted{color:#929caf}.tabs{display:flex;gap:.5rem;flex-wrap:wrap;margin:1.5rem 0}button{background:#232936;color:#dce4f2;border:1px solid #3a4558;border-radius:7px;padding:.55rem .8rem;cursor:pointer}button:hover{background:#30394a}pre{background:#191d25;border:1px solid #303744;border-radius:10px;padding:1rem;overflow:auto;min-height:18rem}
</style></head><body><main><h1>Temper Observe</h1><p class="muted">Local verified specifications</p>
<div class="tabs" id="tabs"></div><pre id="output">Loading…</pre></main><script>
const views=['health','specs','entities','workflows','trajectories','agents','wasm/modules'];
async function load(view){output.textContent='Loading '+view+'…';try{const r=await fetch('/observe/api/'+view,{credentials:'same-origin'});const t=await r.text();if(!r.ok)throw Error(t);try{output.textContent=JSON.stringify(JSON.parse(t),null,2)}catch{output.textContent=t}}catch(e){output.textContent='Observe error: '+e.message}}
for(const view of views){const b=document.createElement('button');b.textContent=view;b.onclick=()=>load(view);tabs.appendChild(b)}load('health');
</script></body></html>"#;

#[derive(Clone)]
struct LocalObserveState {
    api_token: String,
    base_url: String,
    tenant: String,
    nonce: String,
    session: String,
    nonce_used: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct BootstrapQuery {
    nonce: String,
}

pub(super) fn install(
    router: Router,
    port: u16,
    api_token: String,
    tenant: String,
    open_after_start: bool,
) -> Result<(Router, String)> {
    let nonce = random_secret();
    let session = random_secret();
    let bootstrap_url = format!("http://127.0.0.1:{port}/observe/bootstrap?nonce={nonce}");
    let state = LocalObserveState {
        api_token: api_token.clone(),
        base_url: format!("http://127.0.0.1:{port}"),
        tenant,
        nonce,
        session,
        nonce_used: Arc::new(AtomicBool::new(false)),
    };
    let observe_router = Router::new()
        .route("/observe", get(index))
        .route("/observe/bootstrap", get(bootstrap))
        .route("/observe/api/{*path}", get(proxy_observe))
        .with_state(state);
    if open_after_start && let Err(error) = open::that(&bootstrap_url) {
        tracing::warn!(%error, "failed to open local Observe");
    }
    let router = router.merge(observe_router).merge(temper_mcp::http_router(
        temper_mcp::McpConfig {
            temper_port: Some(port),
            temper_url: None,
            agent_id: None,
            agent_type: None,
            session_id: None,
            api_key: Some(api_token),
        },
        port,
    ));
    let summary = format!("\n  Observe: {bootstrap_url}\n  MCP: http://127.0.0.1:{port}/mcp");
    Ok((router, summary))
}

async fn bootstrap(
    State(state): State<LocalObserveState>,
    Query(query): Query<BootstrapQuery>,
) -> Response {
    if query.nonce != state.nonce || state.nonce_used.swap(true, Ordering::SeqCst) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let cookie = format!(
        "temper_local_session={}; HttpOnly; SameSite=Strict; Path=/observe",
        state.session
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to("/observe")).into_response()
}

async fn index(State(state): State<LocalObserveState>, headers: HeaderMap) -> Response {
    if !authenticated(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Html(LOCAL_OBSERVE_HTML).into_response()
}

async fn proxy_observe(
    State(state): State<LocalObserveState>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authenticated(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    const ALLOWED_VIEWS: &[&str] = &[
        "health",
        "specs",
        "entities",
        "workflows",
        "trajectories",
        "agents",
        "wasm/modules",
    ];
    if !ALLOWED_VIEWS.contains(&path.as_str()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let response = match reqwest::Client::new()
        .get(format!("{}/observe/{path}", state.base_url))
        .header("Authorization", format!("Bearer {}", state.api_token))
        .header("X-Tenant-Id", &state.tenant)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return (StatusCode::BAD_GATEWAY, error.to_string()).into_response();
        }
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

fn authenticated(state: &LocalObserveState, headers: &HeaderMap) -> bool {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookies| {
            cookies
                .split(';')
                .any(|cookie| cookie.trim() == format!("temper_local_session={}", state.session))
        })
}

fn random_secret() -> String {
    let mut random = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut random); // determinism-ok: local browser session secret
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}
