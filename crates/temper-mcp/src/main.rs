use std::env;

use temper_mcp::{McpConfig, run_stdio_server};

fn parse_args() -> Result<McpConfig, String> {
    let mut temper_port = None;
    let mut temper_url = None;
    let mut agent_id = None;
    let mut agent_type = None;
    let mut session_id = None;
    let mut api_key = env::var("TEMPER_API_KEY").ok();

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let value = args.next().ok_or("--port requires a value")?;
                let parsed = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port value: {value}"))?;
                temper_port = Some(parsed);
            }
            "--url" => {
                temper_url = Some(args.next().ok_or("--url requires a value")?);
            }
            "--agent-id" => {
                agent_id = Some(args.next().ok_or("--agent-id requires a value")?);
            }
            "--agent-type" => {
                agent_type = Some(args.next().ok_or("--agent-type requires a value")?);
            }
            "--session-id" => {
                session_id = Some(args.next().ok_or("--session-id requires a value")?);
            }
            "--api-key" => {
                api_key = Some(args.next().ok_or("--api-key requires a value")?);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }

    if temper_port.is_some() && temper_url.is_some() {
        return Err("use either --port or --url, not both".to_string());
    }
    if temper_port.is_none() && temper_url.is_none() {
        return Err("either --port or --url is required".to_string());
    }

    Ok(McpConfig {
        temper_port,
        temper_url,
        agent_id,
        agent_type,
        session_id,
        api_key,
    })
}

fn print_help() {
    eprintln!(
        "temper-mcp\n\n\
Usage:\n  temper-mcp --port <PORT> [--agent-id <ID>] [--agent-type <TYPE>] [--session-id <ID>] [--api-key <KEY>]\n  temper-mcp --url <URL> [--agent-id <ID>] [--agent-type <TYPE>] [--session-id <ID>] [--api-key <KEY>]\n\n\
Options:\n  --port <PORT>        Connect to a local Temper server on 127.0.0.1:<PORT>\n  --url <URL>          Connect to a Temper server at the given base URL\n  --agent-id <ID>      Optional local label; does not grant platform identity\n  --agent-type <TYPE>  Optional local type label; does not grant platform identity\n  --session-id <ID>    Set X-Session-Id for outbound requests\n  --api-key <KEY>      Bearer token for API authentication (or use TEMPER_API_KEY)\n  -h, --help           Show this help text"
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = match parse_args() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            eprintln!();
            print_help();
            std::process::exit(2);
        }
    };

    run_stdio_server(config).await?;
    Ok(())
}
