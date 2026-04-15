//! `temper decide` — terminal-based governance decision approval.
//!
//! Connects to a running Temper server, polls for pending decisions,
//! and allows a human to approve or deny them with scope selection.
//! Uses [`PolicyScopeMatrix`] for fine-grained Cedar policy generation.

use std::io::{self, Write};

use anyhow::{Context, Result};
use temper_authz::{ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope};

/// Run the `temper decide` interactive loop.
///
/// Polls the server for pending decisions and prompts the human to
/// approve/deny each one with scope selection.
pub async fn run(port: u16, tenant: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let base_url = format!("http://127.0.0.1:{port}");

    println!("Temper Decide — Governance Terminal");
    println!("  Server: {base_url}");
    println!("  Tenant: {tenant}");
    println!("  Press Ctrl+C to exit");
    println!();

    loop {
        let decisions = fetch_pending_decisions(&client, &base_url, tenant).await?;

        if decisions.is_empty() {
            print!("\r  Waiting for pending decisions...");
            io::stdout().flush()?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }

        println!("\n  {} pending decision(s):", decisions.len());
        println!();

        for decision in &decisions {
            let id = decision["id"].as_str().unwrap_or("unknown");
            let action = decision["action"].as_str().unwrap_or("unknown");
            let resource_type = decision["resource_type"].as_str().unwrap_or("unknown");
            let resource_id = decision["resource_id"].as_str().unwrap_or("unknown");
            let agent_id = decision["agent_id"].as_str().unwrap_or("unknown");

            println!("  Decision: {id}");
            println!("    Agent:     {agent_id}");
            println!("    Action:    {action}");
            println!("    Resource:  {resource_type}::{resource_id}");
            println!();

            print!("  Allow? [n]arrow / [m]edium / [b]road / [d]eny > ");
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let choice = input.trim().to_lowercase();

            let matrix = match choice.as_str() {
                "n" | "narrow" => Some(PolicyScopeMatrix {
                    principal: PrincipalScope::ThisAgent,
                    action: ActionScope::ThisAction,
                    resource: ResourceScope::ThisResource,
                    duration: DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                }),
                "m" | "medium" => Some(PolicyScopeMatrix::default_for(None)),
                "b" | "broad" => Some(PolicyScopeMatrix {
                    principal: PrincipalScope::ThisAgent,
                    action: ActionScope::AllActionsOnType,
                    resource: ResourceScope::AnyOfType,
                    duration: DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                }),
                "d" | "deny" => None,
                _ => {
                    println!("  Skipped (invalid input).");
                    continue;
                }
            };

            match matrix {
                Some(m) => {
                    approve_decision(&client, &base_url, tenant, id, &m).await?;
                    println!(
                        "  Approved with scope: {:?}/{:?}/{:?}",
                        m.principal, m.action, m.resource
                    );
                }
                None => {
                    deny_decision(&client, &base_url, tenant, id).await?;
                    println!("  Denied.");
                }
            }
            println!();
        }
    }
}

async fn fetch_pending_decisions(
    client: &reqwest::Client,
    base_url: &str,
    tenant: &str,
) -> Result<Vec<serde_json::Value>> {
    let url = format!("{base_url}/api/tenants/{tenant}/decisions?status=Pending");
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Failed to connect to Temper server")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Server returned {status}: {body}");
    }

    let body: serde_json::Value = response.json().await.context("Failed to parse response")?;

    // Handle both array and {decisions: [...]} formats.
    if let Some(arr) = body.as_array() {
        Ok(arr.clone())
    } else if let Some(arr) = body.get("decisions").and_then(|v| v.as_array()) {
        Ok(arr.clone())
    } else {
        Ok(vec![])
    }
}

async fn approve_decision(
    client: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    decision_id: &str,
    scope: &PolicyScopeMatrix,
) -> Result<()> {
    let url = format!("{base_url}/api/tenants/{tenant}/decisions/{decision_id}/approve");
    let payload = serde_json::json!({
        "scope": scope,
        "decided_by": "human-terminal"
    });

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("Failed to approve decision")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Approve failed ({status}): {body}");
    }

    Ok(())
}

async fn deny_decision(
    client: &reqwest::Client,
    base_url: &str,
    tenant: &str,
    decision_id: &str,
) -> Result<()> {
    let url = format!("{base_url}/api/tenants/{tenant}/decisions/{decision_id}/deny");

    let response = client
        .post(&url)
        .send()
        .await
        .context("Failed to deny decision")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Deny failed ({status}): {body}");
    }

    Ok(())
}
