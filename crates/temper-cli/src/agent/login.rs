//! `temper login openai` — OAuth PKCE flow for OpenAI Codex.
//!
//! Opens the user's browser to authenticate with their ChatGPT Plus/Pro subscription,
//! captures the callback on `localhost:1455`, exchanges the code for tokens, and saves
//! credentials to `~/.temper/codex-auth.json`.

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use base64::Engine;
use sha2::Digest;
use tokio::sync::oneshot;

use temper_agent_runtime::providers::codex::auth;

/// Run the OAuth login flow for the given provider.
pub async fn run(provider: &str) -> Result<()> {
    match provider {
        "openai" => run_openai_login().await,
        other => anyhow::bail!("Unknown provider: {other}. Supported: openai"),
    }
}

/// Execute the OpenAI Codex PKCE OAuth flow.
async fn run_openai_login() -> Result<()> {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // 1. Generate PKCE verifier + challenge.
    let verifier_bytes: [u8; 32] = rand::random(); // determinism-ok: CLI auth, not simulation-visible
    let verifier = encoder.encode(verifier_bytes);
    let challenge_hash = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = encoder.encode(challenge_hash);

    // 2. Generate state parameter.
    let state_bytes: [u8; 16] = rand::random(); // determinism-ok: CLI auth
    let state = hex::encode(state_bytes);

    // 3. Build authorization URL.
    let authorize_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256&originator=temper&codex_cli_simplified_flow=true",
        auth::AUTHORIZE_URL,
        urlencoding::encode(auth::CLIENT_ID),
        urlencoding::encode(auth::REDIRECT_URI),
        urlencoding::encode(auth::SCOPES),
        urlencoding::encode(&state),
        urlencoding::encode(&challenge),
    );

    println!("Opening browser for OpenAI authentication...");
    println!();
    println!("If the browser doesn't open, visit this URL:");
    println!("{authorize_url}");
    println!();

    // Try to open the browser.
    let _ = open::that(&authorize_url);

    // 4. Start local callback server.
    let (tx, rx) = oneshot::channel::<(String, String)>();
    let tx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let app = axum::Router::new().route(
        "/auth/callback",
        get({
            let tx = tx.clone();
            move |Query(params): Query<std::collections::HashMap<String, String>>| {
                let tx = tx.clone();
                async move {
                    let code = params.get("code").cloned().unwrap_or_default();
                    let cb_state = params.get("state").cloned().unwrap_or_default();
                    if let Some(sender) = tx.lock().await.take() {
                        let _ = sender.send((code, cb_state));
                    }
                    Html(
                        "<html><body><h2>Login successful!</h2>\
                         <p>You can close this tab and return to the terminal.</p></body></html>"
                            .to_string(),
                    )
                }
            }
        }),
    );

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:1455").await {
        Ok(l) => l,
        Err(e) => {
            // Fallback: ask the user to paste the callback URL manually.
            eprintln!("Could not bind to port 1455: {e}");
            eprintln!("After authenticating in the browser, paste the full callback URL here:");
            let mut url_input = String::new();
            std::io::stdin().read_line(&mut url_input)?; // determinism-ok: CLI interactive
            let url_input = url_input.trim();
            let parsed = url::Url::parse(url_input).context("Invalid URL")?;
            let params: std::collections::HashMap<String, String> =
                parsed.query_pairs().into_owned().collect();
            let code = params
                .get("code")
                .context("Missing 'code' parameter")?
                .clone();
            let cb_state = params
                .get("state")
                .context("Missing 'state' parameter")?
                .clone();
            return finish_login(&verifier, &state, &code, &cb_state).await;
        }
    };

    println!("Waiting for OAuth callback on http://127.0.0.1:1455 ...");

    // 5. Serve and wait with timeout.
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .ok();
    });

    let (code, cb_state) = tokio::select! {
        result = rx => {
            result.context("Callback channel closed unexpectedly")?
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(120)) => {  // determinism-ok: CLI timeout
            anyhow::bail!("Login timed out after 120 seconds. Please try again.");
        }
    };

    server.abort();

    finish_login(&verifier, &state, &code, &cb_state).await
}

/// Validate state, exchange code for tokens, and save credentials.
async fn finish_login(verifier: &str, expected_state: &str, code: &str, state: &str) -> Result<()> {
    // 6. Validate state.
    if state != expected_state {
        anyhow::bail!("OAuth state mismatch — possible CSRF attack. Please try again.");
    }

    // 7. Exchange code for tokens.
    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", auth::CLIENT_ID),
        ("code", code),
        ("redirect_uri", auth::REDIRECT_URI),
        ("code_verifier", verifier),
    ];

    let resp = client
        .post(auth::TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("Failed to exchange authorization code for tokens")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed ({status}): {body}");
    }

    let token_resp: serde_json::Value = resp.json().await.context("Failed to parse token response")?;

    let access_token = token_resp["access_token"]
        .as_str()
        .context("Missing access_token in token response")?
        .to_string();

    let refresh_token = token_resp["refresh_token"]
        .as_str()
        .context("Missing refresh_token in token response")?
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let now = std::time::SystemTime::now() // determinism-ok: CLI auth
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // 8. Decode account_id from JWT.
    let account_id = auth::decode_account_id(&access_token)?;

    // 9. Save credentials.
    let creds = auth::CodexCredentials {
        refresh_token,
        access_token,
        expires_at: now + expires_in,
        account_id,
    };
    auth::save_credentials(&creds)?;

    println!();
    println!("Authenticated with OpenAI successfully.");
    println!("You can now use: temper agent --model gpt-4.1-2025-04-14 --goal \"...\"");

    Ok(())
}
