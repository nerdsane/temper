use anyhow::Result;

pub async fn run(
    port: Option<u16>,
    url: Option<String>,
    apps: Vec<(String, String)>,
    agent_id: Option<String>,
) -> Result<()> {
    let apps = apps
        .into_iter()
        .map(|(name, specs_dir)| temper_mcp::AppConfig {
            name,
            specs_dir: specs_dir.into(),
        })
        .collect();

    temper_mcp::run_stdio_server(temper_mcp::McpConfig {
        temper_port: port,
        temper_url: url,
        apps,
        principal_id: agent_id.or_else(|| Some("mcp-agent".to_string())),
        api_key: std::env::var("TEMPER_API_KEY").ok(),
    })
    .await
}
