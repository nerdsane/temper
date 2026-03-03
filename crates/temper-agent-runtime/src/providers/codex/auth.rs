//! OAuth credential management for OpenAI Codex.
//!
//! Handles loading, saving, and refreshing OAuth tokens obtained via
//! `temper login openai`. Credentials are stored at `~/.temper/codex-auth.json`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// OpenAI OAuth application client ID (Codex CLI, Apache 2.0).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// OpenAI OAuth authorization endpoint.
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// OpenAI OAuth token endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Local redirect URI for the PKCE callback.
pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
/// OAuth scopes requested.
pub const SCOPES: &str = "openid profile email offline_access";
/// OpenAI Codex Responses API base URL.
pub const API_BASE: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Persisted OAuth credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexCredentials {
    /// OAuth refresh token for obtaining new access tokens.
    pub refresh_token: String,
    /// Current access token (JWT).
    pub access_token: String,
    /// Unix timestamp (seconds) when `access_token` expires.
    pub expires_at: i64,
    /// ChatGPT account ID extracted from the JWT.
    pub account_id: String,
}

/// Return the path to the credentials file: `~/.temper/codex-auth.json`.
pub fn credentials_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".temper").join("codex-auth.json"))
}

/// Load credentials from disk. Returns `None` if the file does not exist.
pub fn load_credentials() -> Result<Option<CodexCredentials>> {
    let path = credentials_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(&path) // determinism-ok: CLI credential store, not simulation-visible
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let creds: CodexCredentials =
        serde_json::from_str(&data).context("Failed to parse codex-auth.json")?;
    Ok(Some(creds))
}

/// Save credentials to disk with restrictive permissions (0600).
pub fn save_credentials(creds: &CodexCredentials) -> Result<()> {
    let path = credentials_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent) // determinism-ok: CLI credential store
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(creds)?;
    std::fs::write(&path, &json) // determinism-ok: CLI credential store
        .with_context(|| format!("Failed to write {}", path.display()))?;

    // Set file mode to 0600 (owner read/write only) on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Check whether credentials are expired (with 60-second buffer).
pub fn is_expired(creds: &CodexCredentials) -> bool {
    let now = std::time::SystemTime::now() // determinism-ok: CLI auth, not simulation-visible
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    creds.expires_at - 60 <= now
}

/// Refresh an access token using the refresh token.
pub async fn refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<CodexCredentials> {
    let params = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ];

    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .context("Failed to refresh OpenAI token")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed ({status}): {body}");
    }

    let token_resp: serde_json::Value = resp.json().await.context("Failed to parse token JSON")?;

    let access_token = token_resp["access_token"]
        .as_str()
        .context("Missing access_token in refresh response")?
        .to_string();

    let new_refresh = token_resp["refresh_token"]
        .as_str()
        .unwrap_or(refresh_token)
        .to_string();

    let expires_in = token_resp["expires_in"].as_i64().unwrap_or(3600);
    let now = std::time::SystemTime::now() // determinism-ok: CLI auth
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let account_id = decode_account_id(&access_token)?;

    let creds = CodexCredentials {
        refresh_token: new_refresh,
        access_token,
        expires_at: now + expires_in,
        account_id,
    };

    save_credentials(&creds)?;
    Ok(creds)
}

/// Decode the ChatGPT account ID from a JWT access token.
///
/// The token's middle segment (base64url-encoded JSON payload) contains
/// `{"https://api.openai.com/auth": {"chatgpt_account_id": "..."}}`.
pub fn decode_account_id(access_token: &str) -> Result<String> {
    use base64::Engine;

    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() < 3 {
        anyhow::bail!("Invalid JWT: expected 3 segments, got {}", parts.len());
    }

    let payload_b64 = parts[1];
    let decoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload_bytes = decoder
        .decode(payload_b64)
        .context("Failed to base64-decode JWT payload")?;

    let payload: serde_json::Value =
        serde_json::from_slice(&payload_bytes).context("Failed to parse JWT payload JSON")?;

    let account_id = payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
        .context("JWT missing chatgpt_account_id in https://api.openai.com/auth claim")?
        .to_string();

    Ok(account_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credentials_round_trip() {
        let creds = CodexCredentials {
            refresh_token: "rt_test".into(),
            access_token: "at_test".into(),
            expires_at: 1700000000,
            account_id: "acct_123".into(),
        };
        let json = serde_json::to_string(&creds).unwrap();
        let parsed: CodexCredentials = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.refresh_token, "rt_test");
        assert_eq!(parsed.account_id, "acct_123");
        assert_eq!(parsed.expires_at, 1700000000);
    }

    #[test]
    fn test_is_expired_future() {
        let creds = CodexCredentials {
            refresh_token: "rt".into(),
            access_token: "at".into(),
            expires_at: i64::MAX,
            account_id: "acct".into(),
        };
        assert!(!is_expired(&creds));
    }

    #[test]
    fn test_is_expired_past() {
        let creds = CodexCredentials {
            refresh_token: "rt".into(),
            access_token: "at".into(),
            expires_at: 0,
            account_id: "acct".into(),
        };
        assert!(is_expired(&creds));
    }

    #[test]
    fn test_decode_account_id() {
        use base64::Engine;
        let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let payload = serde_json::json!({
            "sub": "user-123",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_abc123"
            }
        });
        let payload_b64 = encoder.encode(payload.to_string().as_bytes());
        // Construct a fake JWT: header.payload.signature
        let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload_b64}.fakesig");

        let account_id = decode_account_id(&fake_jwt).unwrap();
        assert_eq!(account_id, "acct_abc123");
    }

    #[test]
    fn test_decode_account_id_invalid_jwt() {
        assert!(decode_account_id("not-a-jwt").is_err());
    }

    #[test]
    fn test_decode_account_id_missing_claim() {
        use base64::Engine;
        let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = serde_json::json!({"sub": "user-123"});
        let payload_b64 = encoder.encode(payload.to_string().as_bytes());
        let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload_b64}.fakesig");
        assert!(decode_account_id(&fake_jwt).is_err());
    }
}
