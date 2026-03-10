use anyhow::Result;

pub async fn run(port: Option<u16>, url: Option<String>, agent_id: Option<String>) -> Result<()> {
    temper_mcp::run_stdio_server(temper_mcp::McpConfig {
        temper_port: port,
        temper_url: url,
        principal_id: agent_id.or_else(|| Some("mcp-agent".to_string())),
        api_key: std::env::var("TEMPER_API_KEY").ok(),
    })
    .await
}
