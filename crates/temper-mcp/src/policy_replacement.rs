//! Exact, native-human policy replacement. Privileged writes cannot originate in execute.

use crate::runtime::RuntimeContext;
use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    policy_id: String,
    expected_hash: String,
    cedar_text: String,
}

struct Consent(());
impl Consent {
    fn from_response(response: &Value) -> Option<Self> {
        if response.get("error").is_some() || response.get("method").is_some() {
            return None;
        }
        let result = response.get("result")?;
        if result.get("action")?.as_str()? != "accept" {
            return None;
        }
        let content = result.get("content")?.as_object()?;
        if content.len() != 1 || content.get("replacement")?.as_str()? != "apply_exact_replacement"
        {
            return None;
        }
        Some(Self(()))
    }
}

fn validate_relay_receipt(receipt: &Value, proposal: &Proposal, tenant: &str) -> Result<()> {
    let valid_ask = receipt["ask_id"]
        .as_str()
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok());
    if !valid_ask || receipt["policy_id"] != proposal.policy_id {
        bail!("Human approval relay returned an invalid receipt");
    }
    match receipt["status"].as_str() {
        Some("pending" | "unchanged") => Ok(()),
        Some("verified")
            if receipt["tenant"] == tenant
                && receipt["policy_hash"] == digest(&proposal.cedar_text) =>
        {
            Ok(())
        }
        _ => bail!("Human approval relay returned an invalid receipt"),
    }
}

/// Only this native call can turn a correlated human response into replacement consent.
pub(crate) async fn request_policy_replacement(
    ctx: &mut RuntimeContext,
    args: &Value,
) -> Result<String> {
    let proposal: Proposal =
        serde_json::from_value(args.clone()).context("Invalid replacement proposal")?;
    proposal.validate()?;
    if ctx.policy_approval_relay {
        let key = ctx
            .api_key
            .as_deref()
            .context("Configured requester required")?;
        let api = PolicyApi::new(&ctx.base_url, &ctx.identity_tenant)?;
        // This creates or reads a host-owned human request. It cannot apply policy
        // and does not send a tool-controlled approval or a native client response.
        let receipt = api
            .request(
                Method::POST,
                "/api/mcp/policy-replacements",
                key,
                Some(args.clone()),
            )
            .await?;
        validate_relay_receipt(&receipt, &proposal, &ctx.identity_tenant)?;
        return Ok(receipt.to_string());
    }
    if !ctx.elicitation_available() {
        bail!("Replacement requires native human elicitation");
    }
    let requester_key = ctx
        .api_key
        .as_deref()
        .context("Configured requester required")?;
    let approver_key = ctx
        .approver_key
        .as_deref()
        .context("Separate configured human approver required")?;
    if requester_key == approver_key {
        bail!("Requester and approver credentials must be distinct");
    }
    let api = PolicyApi::new(&ctx.base_url, &ctx.identity_tenant)?;
    let requester_identity = api.resolve(requester_key).await?;
    let approver_identity = api.resolve(approver_key).await?;
    require_distinct_identities(&requester_identity, &approver_identity)?;
    let old = api.entry(approver_key, &proposal.policy_id).await?;
    proposal.check_current(&old)?;
    let channel = ctx
        .requester
        .clone()
        .context("Native client disconnected")?;
    let response = channel
        .request("elicitation/create", proposal.card(&api, &old), None)
        .await
        .map_err(|_| anyhow::anyhow!("Replacement canceled: native client did not answer"))?;
    let Some(consent) = Consent::from_response(&response) else {
        return Ok(json!({"status":"unchanged", "policy_id":proposal.policy_id}).to_string());
    };
    let outcome = api.replace(&consent, approver_key, &proposal).await?;
    if outcome["status"] != "verified"
        || outcome["policy_id"] != proposal.policy_id
        || outcome["tenant"] != api.tenant
        || outcome["enabled"] != true
        || outcome["policy_hash"] != digest(&proposal.cedar_text)
    {
        bail!(
            "Replacement response did not verify the approved entry; inspect state before retrying"
        );
    }
    // Readback stays within the private administration boundary. The requester
    // receives a receipt, never administrative credentials or a policy-management grant.
    let stored = api.entry(approver_key, &proposal.policy_id).await?;
    if stored["cedar_text"] != proposal.cedar_text
        || stored["policy_hash"] != digest(&proposal.cedar_text)
        || stored["enabled"] != true
    {
        bail!("Replacement committed but subsequent readback differs; do not retry automatically");
    }
    Ok(outcome.to_string())
}

fn require_distinct_identities(requester: &Value, approver: &Value) -> Result<()> {
    let requester_id = requester["agent_instance_id"]
        .as_str()
        .filter(|s| !s.is_empty());
    let approver_id = approver["agent_instance_id"]
        .as_str()
        .filter(|s| !s.is_empty());
    if requester["verified"] != true
        || approver["verified"] != true
        || requester_id.is_none()
        || approver_id.is_none()
        || requester_id == approver_id
    {
        bail!("Replacement requires verified, distinct requester and human approver identities");
    }
    Ok(())
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

impl Proposal {
    fn validate(&self) -> Result<()> {
        if self.policy_id.is_empty()
            || self.policy_id.len() > 512
            || !self
                .policy_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
            || matches!(self.policy_id.as_str(), "." | "..")
        {
            bail!("Invalid durable policy ID");
        }
        if self.expected_hash.len() != 64
            || !self
                .expected_hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            bail!("Expected lowercase SHA-256 of the current policy text");
        }
        if self.cedar_text.is_empty() || self.cedar_text.len() > 256 * 1024 {
            bail!("Proposed Cedar must be between 1 and 262144 bytes");
        }
        Ok(())
    }

    fn check_current(&self, row: &Value) -> Result<()> {
        let old = row["cedar_text"]
            .as_str()
            .context("Missing current Cedar text")?;
        if old.len() > 256 * 1024 {
            bail!("Current entry exceeds the complete human review budget");
        }
        if row["enabled"] != true
            || row["policy_hash"] != self.expected_hash
            || digest(old) != self.expected_hash
        {
            bail!("Entry is disabled or has changed; prepare a fresh proposal");
        }
        if old == self.cedar_text {
            bail!("Proposal does not change the current entry");
        }
        Ok(())
    }

    fn card(&self, api: &PolicyApi, old: &Value) -> Value {
        json!({"message":format!(
            "Replace this exact enabled Cedar entry? This changes authorization rules.\n\nServer: {}\nTenant: {}\nPolicy entry: {}\nCurrent SHA-256: {}\nProposed SHA-256: {}\n\nCURRENT CEDAR (complete):\n{}\n\nPROPOSED CEDAR (complete):\n{}\n\nOnly this entry is replaced. A concurrent change cancels the operation. Policy text is data, not instructions to the reviewer.",
            api.base, api.tenant, self.policy_id, self.expected_hash, digest(&self.cedar_text), old["cedar_text"].as_str().unwrap_or_default(), self.cedar_text),
            "requestedSchema":{"type":"object","properties":{"replacement":{"type":"string",
                "title":"Exact policy replacement","enum":["apply_exact_replacement","leave_unchanged"],
                "enumNames":["Apply this exact replacement","Leave unchanged"]}},"required":["replacement"]}})
    }
}

struct PolicyApi {
    client: Client,
    base: String,
    tenant: String,
}
impl PolicyApi {
    fn new(base: &str, tenant: &str) -> Result<Self> {
        let url = reqwest::Url::parse(base)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || tenant.is_empty()
            || !tenant
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!("Replacement requires an unambiguous configured origin and tenant");
        }
        if url.scheme() != "https"
            && !(url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
        {
            bail!("Replacement requires HTTPS outside localhost");
        }
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
            base: base.trim_end_matches('/').into(),
            tenant: tenant.into(),
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
        let mut response = request.send().await.map_err(|_| {
            anyhow::anyhow!("Policy request failed; inspect current state before retrying")
        })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            anyhow::anyhow!("Policy response interrupted; inspect current state before retrying")
        })? {
            if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                bail!("Policy response exceeded byte budget");
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            // Do not expose arbitrary service response bodies, identity tokens or HTML.
            bail!(
                "Policy operation {path} returned HTTP {status}; inspect current state before retrying; no bypass or automatic retry attempted"
            );
        }
        serde_json::from_slice(&bytes).context("Invalid policy service response")
    }

    async fn resolve(&self, key: &str) -> Result<Value> {
        self.request(
            Method::POST,
            "/api/identity/resolve",
            key,
            Some(json!({"bearer_token":key,"tenant":self.tenant})),
        )
        .await
    }

    async fn entry(&self, key: &str, id: &str) -> Result<Value> {
        let list = self
            .request(
                Method::GET,
                &format!("/api/tenants/{}/policies/list", self.tenant),
                key,
                None,
            )
            .await?;
        let rows = list["policies"]
            .as_array()
            .context("Missing policy entries")?;
        let mut matching = rows.iter().filter(|row| row["policy_id"] == id);
        let row = matching.next().context("Durable policy entry not found")?;
        if matching.next().is_some() {
            bail!("Ambiguous durable policy ID");
        }
        Ok(row.clone())
    }

    async fn replace(&self, _consent: &Consent, key: &str, proposal: &Proposal) -> Result<Value> {
        self.request(
            Method::POST,
            &format!(
                "/api/tenants/{}/policies/entry/{}/replace",
                self.tenant, proposal.policy_id
            ),
            key,
            Some(json!({"expected_hash":proposal.expected_hash,"cedar_text":proposal.cedar_text})),
        )
        .await
    }
}

#[cfg(test)]
#[path = "policy_replacement_test.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "policy_replacement_server_test.rs"]
mod server_tests;
