use anyhow::Result;

pub async fn run(port: u16, apps: Vec<(String, String)>) -> Result<()> {
    let apps = apps
        .into_iter()
        .map(|(name, specs_dir)| temper_mcp::AppConfig {
            name,
            specs_dir: specs_dir.into(),
        })
        .collect();

    temper_mcp::run_stdio_server(temper_mcp::McpConfig {
        temper_port: port,
        apps,
        principal_id: Some("mcp-agent".to_string()),
    })
    .await
}
