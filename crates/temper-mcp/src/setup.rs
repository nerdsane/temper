//! Explicit human administration through the native MCP client channel.

use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;

use crate::runtime::RuntimeContext;
use crate::setup_consent::{SetupConsent, setup_params};
use crate::setup_identity::{RequesterIdentity, load_identity, save_identity};

const REQUESTER_TYPE: &str = "mcp-harness";
const OPERATOR_IDENTITY_POLICY: &str = r#"permit(
  principal is Agent,
  action in [Action::"create", Action::"read", Action::"list", Action::"Define", Action::"Issue"],
  resource
) when {
  principal has agent_type && principal.agent_type == "operator" &&
  principal has agentTypeVerified && principal.agentTypeVerified == true &&
  (resource is AgentType || resource is AgentCredential)
};"#;

/// No tool-controlled arguments may influence a setup grant or its destination.
pub(crate) async fn setup_connection(
    ctx: &mut RuntimeContext,
    arguments: &Value,
) -> Result<String> {
    if !arguments.as_object().is_some_and(|args| args.is_empty()) {
        bail!("setup_connection accepts no arguments");
    }
    if !ctx.elicitation_available() {
        bail!("Setup requires a connected human client with native elicitation enabled");
    }
    let path = ctx
        .identity_file
        .clone()
        .context("Configure TEMPER_MCP_IDENTITY_FILE before setup")?;
    setup_at(ctx, &path).await
}

async fn setup_at(ctx: &mut RuntimeContext, path: &std::path::Path) -> Result<String> {
    let operator_key = ctx
        .approver_key
        .as_ref()
        .or(ctx.api_key.as_ref())
        .context("Setup requires a configured service operator credential")?
        .clone();
    let admin = SetupAdmin::new(&ctx.base_url, &ctx.identity_tenant, operator_key)?;
    let operator = admin.resolve(&admin.key).await?;
    if operator["verified"] != true || operator["agent_type_name"] != "operator" {
        bail!("Configured setup credential is not a verified service operator");
    }
    let requester = ctx
        .requester
        .clone()
        .context("Native client disconnected")?;
    let response = requester
        .request(
            "elicitation/create",
            setup_params(&ctx.base_url, &ctx.identity_tenant),
            None,
        )
        .await
        .map_err(|_| anyhow::anyhow!("Setup canceled: no human response was received"))?;
    let Some(consent) = SetupConsent::from_client_response(&response) else {
        return Ok(json!({"status":"unchanged", "note":"No setup was authorized"}).to_string());
    };
    let identity = prepare_identity(&consent, path, &ctx.base_url, &ctx.identity_tenant)?;
    admin.provision(&consent, &identity).await?;
    let resolved = admin.resolve(&identity.token).await?;
    if resolved["verified"] != true
        || resolved["agent_type_name"] != REQUESTER_TYPE
        || resolved["agent_instance_id"] != identity.principal
        || resolved["agent_instance_id"] == operator["agent_instance_id"]
    {
        bail!("Setup did not resolve to the expected distinct nonoperator identity");
    }
    ctx.approver_key = Some(admin.key);
    ctx.api_key = Some(identity.token);
    ctx.agent_id = Some(identity.principal.clone());
    ctx.agent_type = Some(REQUESTER_TYPE.into());
    Ok(
        json!({"status":"configured", "requester": identity.principal,
            "tenant":ctx.identity_tenant, "server":ctx.base_url,
            "note":"No pending decisions were approved. Subsequent actions use normal governance."
        })
        .to_string(),
    )
}

#[cfg(all(test, unix))]
#[path = "setup_test.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "setup_server_test.rs"]
mod server_tests;

pub(crate) fn identity_path() -> Result<Option<PathBuf>> {
    let Some(raw) = std::env::var_os("TEMPER_MCP_IDENTITY_FILE") else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        bail!("TEMPER_MCP_IDENTITY_FILE must be absolute");
    }
    Ok(Some(path))
}

fn prepare_identity(
    consent: &SetupConsent,
    path: &std::path::Path,
    server: &str,
    tenant: &str,
) -> Result<RequesterIdentity> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return load_identity(path, server, tenant),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // Two independent v4 UUIDs provide 244 random bits; never use the simulator RNG.
    let identity = RequesterIdentity {
        server: server.into(),
        tenant: tenant.into(),
        principal: format!("mcp-{}", uuid::Uuid::new_v4().simple()),
        token: format!(
            "tmpr_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
    };
    save_identity(consent, path, &identity)?;
    Ok(identity)
}

/// The operator key is confined to this module; no Debug or serialized form.
struct SetupAdmin {
    client: Client,
    base: String,
    tenant: String,
    key: String,
}

impl SetupAdmin {
    fn new(base: &str, tenant: &str, key: String) -> Result<Self> {
        let url = reqwest::Url::parse(base)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || !tenant
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            || tenant.is_empty()
        {
            bail!("Setup requires an unambiguous configured server origin and tenant");
        }
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        {
            bail!("Setup requires HTTPS outside localhost");
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
            base: base.into(),
            tenant: tenant.into(),
            key,
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(key)
            .header("X-Tenant-Id", &self.tenant);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| anyhow::anyhow!(
            "Setup request failed; outcome may be unknown. Retained private state supports explicit recovery"
        ))?;
        if !response.status().is_success() {
            bail!(
                "Setup operation {path} returned HTTP {}; no policy bypass or automatic retry was attempted",
                response.status()
            );
        }
        read_response(response).await
    }

    async fn resolve(&self, token: &str) -> Result<Value> {
        self.request(
            Method::POST,
            "/api/identity/resolve",
            token,
            Some(json!({"bearer_token":token,"tenant":self.tenant})),
        )
        .await
    }

    async fn provision(&self, _consent: &SetupConsent, identity: &RequesterIdentity) -> Result<()> {
        let policy_path = format!("/api/tenants/{}/policies", self.tenant);
        let existing = self
            .request(Method::GET, &policy_path, &self.key, None)
            .await?;
        let text = existing["policy_text"]
            .as_str()
            .context("Missing policy text")?;
        if !text.contains(OPERATOR_IDENTITY_POLICY) {
            self.request(
                Method::POST,
                &format!("{policy_path}/rules"),
                &self.key,
                Some(json!({"rule":OPERATOR_IDENTITY_POLICY})),
            )
            .await?;
        }
        self.ensure_type().await?;
        self.ensure_credential(identity).await
    }

    async fn ensure_type(&self) -> Result<()> {
        if let Some(entity) = self
            .entity(&format!("/tdata/AgentTypes('{REQUESTER_TYPE}')"))
            .await?
        {
            if entity["status"] == "Active" && entity["fields"]["name"] == REQUESTER_TYPE {
                return Ok(());
            }
            bail!("Existing requester type is incompatible; setup will not overwrite it");
        }
        self.request(
            Method::POST,
            &format!("/tdata/AgentTypes('{REQUESTER_TYPE}')/Temper.Define"),
            &self.key,
            Some(
                json!({"name":REQUESTER_TYPE,"system_prompt":"External MCP requester",
                "tool_set":"none","model":"none","max_turns":"0",
                "adapter_config":"{}","default_budget_cents":"0"}),
            ),
        )
        .await?;
        Ok(())
    }

    async fn ensure_credential(&self, identity: &RequesterIdentity) -> Result<()> {
        let hash = format!("{:x}", Sha256::digest(identity.token.as_bytes()));
        if let Some(entity) = self
            .entity(&format!("/tdata/AgentCredentials('{hash}')"))
            .await?
        {
            let fields = &entity["fields"];
            if entity["status"] != "Active" {
                bail!("Existing requester credential is inactive; setup will not reactivate it");
            }
            if fields["key_hash"] == hash
                && fields["agent_instance_id"] == identity.principal
                && fields["agent_type_id"] == REQUESTER_TYPE
            {
                return Ok(());
            }
            bail!(
                "Existing requester credential has unexpected bindings; setup will not overwrite it"
            );
        }
        self.request(
            Method::POST,
            &format!("/tdata/AgentCredentials('{hash}')/Temper.Issue"),
            &self.key,
            Some(
                json!({"agent_type_id":REQUESTER_TYPE,"agent_instance_id":identity.principal,
                "key_hash":hash,"key_prefix":identity.token.chars().take(8).collect::<String>(),
                "description":"Human-authorized MCP requester","created_by":"operator",
                "expires_at":""}),
            ),
        )
        .await?;
        Ok(())
    }

    async fn entity(&self, path: &str) -> Result<Option<Value>> {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.key)
            .header("X-Tenant-Id", &self.tenant)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!("Identity read returned HTTP {}", response.status());
        }
        Ok(Some(read_response(response).await?))
    }
}

async fn read_response(mut response: reqwest::Response) -> Result<Value> {
    const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("Setup response exceeded its byte budget");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid setup service response"))
}
