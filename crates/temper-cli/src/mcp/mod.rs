use anyhow::Result;

pub async fn run(port: Option<u16>, url: Option<String>, agent_id: Option<String>) -> Result<()> {
    let api_key = std::env::var("TEMPER_API_KEY").ok(); // determinism-ok: startup config

    // Warn if connecting to a remote server without an API key.
    if api_key.is_none() {
        let is_remote = url.as_deref().is_some_and(|u| {
            !u.contains("localhost") && !u.contains("127.0.0.1") && !u.contains("[::1]")
        });
        if is_remote {
            eprintln!(
                "temper-mcp: WARNING — no TEMPER_API_KEY set for remote server.\n\
                 Requests will likely fail with 401 Unauthorized.\n\
                 Set it with:\n\
                 \n\
                 claude mcp add temper -e TEMPER_API_KEY=sk-xxx -- npx -y temper-mcp --url {}\n",
                url.as_deref().unwrap_or("https://api.temper.build")
            );
        }
    }

    // Agent identity is now derived from the MCP `initialize` handshake
    // (clientInfo.name + clientInfo.version + hostname + timestamp).
    // The --agent-id CLI flag is still supported as an explicit override.
    temper_mcp::run_stdio_server(temper_mcp::McpConfig {
        temper_port: port,
        temper_url: url,
        agent_id,
        agent_type: None,
        session_id: None,
        api_key,
    })
    .await
}
