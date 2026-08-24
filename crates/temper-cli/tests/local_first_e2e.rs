use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::{Child, Command};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_bundle_survives_restart_without_workspace() -> Result<()> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("local-proof");
    let moved_workspace = root.path().join("workspace-removed");
    let data_dir = root.path().join("data");
    let port = available_port()?;
    let binary = env!("CARGO_BIN_EXE_temper");

    let init = Command::new(binary)
        .arg("init")
        .arg(&workspace)
        .status()
        .await?;
    anyhow::ensure!(init.success(), "temper init failed");

    let mut daemon = start_daemon(binary, port, &data_dir)?;
    wait_ready(port).await?;
    let install = Command::new(binary)
        .arg("app")
        .arg("install")
        .arg(&workspace)
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}"))
        .arg("--data-dir")
        .arg(&data_dir)
        .env_remove("TEMPER_API_KEY")
        .status()
        .await?;
    anyhow::ensure!(install.success(), "local bundle install failed");

    let token = std::fs::read_to_string(data_dir.join("credentials/operator.token"))?;
    let client = reqwest::Client::new();
    for view in [
        "health",
        "specs",
        "entities",
        "workflows",
        "trajectories",
        "agents",
        "wasm/modules",
    ] {
        let response = client
            .get(format!("http://127.0.0.1:{port}/observe/{view}"))
            .bearer_auth(token.trim())
            .header("X-Tenant-Id", "default")
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "local Observe view '{view}' failed: {}",
            response.text().await?
        );
    }
    let mcp = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(token.trim())
        .header("MCP-Protocol-Version", "2026-07-28")
        .header("Mcp-Method", "tools/list")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
            "params": {}
        }))
        .send()
        .await?;
    anyhow::ensure!(
        mcp.status().is_success(),
        "local HTTP MCP failed: {}",
        mcp.text().await?
    );
    let created = client
        .post(format!("http://127.0.0.1:{port}/tdata/Items"))
        .bearer_auth(token.trim())
        .header("X-Tenant-Id", "default")
        .json(&serde_json::json!({ "Name": "restart proof" }))
        .send()
        .await?;
    anyhow::ensure!(
        created.status().is_success(),
        "create failed: {}",
        created.text().await?
    );
    let created: Value = created.json().await?;
    let entity_id = created["entity_id"]
        .as_str()
        .context("create response omitted entity_id")?;
    let completed = client
        .post(format!(
            "http://127.0.0.1:{port}/tdata/Items('{entity_id}')/Temper.LocalProof.Complete"
        ))
        .bearer_auth(token.trim())
        .header("X-Tenant-Id", "default")
        .json(&serde_json::json!({}))
        .send()
        .await?;
    anyhow::ensure!(
        completed.status().is_success(),
        "action failed: {}",
        completed.text().await?
    );
    let completed: Value = completed.json().await?;
    anyhow::ensure!(
        completed["status"] == "Done",
        "action did not complete entity: {completed}"
    );
    // Entity event persistence is asynchronous relative to the HTTP response.
    tokio::time::sleep(Duration::from_secs(1)).await;

    std::fs::rename(&workspace, &moved_workspace)?;
    daemon.kill().await?;
    let _ = daemon.wait().await;
    let mut restarted = start_daemon(binary, port, &data_dir)?;
    wait_ready(port).await?;
    let restored = client
        .get(format!(
            "http://127.0.0.1:{port}/tdata/Items('{entity_id}')"
        ))
        .bearer_auth(token.trim())
        .header("X-Tenant-Id", "default")
        .send()
        .await?;
    anyhow::ensure!(
        restored.status().is_success(),
        "restored read failed: {}",
        restored.text().await?
    );
    let restored: Value = restored.json().await?;
    anyhow::ensure!(
        restored["status"] == "Done",
        "restored state drifted: {restored}"
    );
    let after_restart = client
        .post(format!("http://127.0.0.1:{port}/tdata/Items"))
        .bearer_auth(token.trim())
        .header("X-Tenant-Id", "default")
        .json(&serde_json::json!({ "Name": "post-restart invocation" }))
        .send()
        .await?;
    anyhow::ensure!(
        after_restart.status().is_success(),
        "post-restart invocation failed: {}",
        after_restart.text().await?
    );
    restarted.kill().await?;
    let _ = restarted.wait().await;
    Ok(())
}

fn start_daemon(binary: &str, port: u16, data_dir: &std::path::Path) -> Result<Child> {
    Command::new(binary)
        .arg("up")
        .arg("--port")
        .arg(port.to_string())
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--no-open")
        .env("SANDBOX_URL", "http://127.0.0.1:9")
        .env("TURSO_PLATFORM_URL", "libsql://must-not-be-used.invalid")
        .env("TURSO_URL", "libsql://must-not-be-used.invalid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn local daemon")
}

async fn wait_ready(port: u16) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    for _ in 0..300 {
        if reqwest::get(&url)
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("daemon did not become ready at {url}")
}

fn available_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}
