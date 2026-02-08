//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the full conversational development
//! experience: developer chat, production chat, and OData API.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use temper_observe::otel::init_tracing;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;

/// Run the `temper serve` command.
///
/// Starts the Temper platform server with developer and/or production modes.
pub async fn run(
    port: u16,
    dev: bool,
    production: bool,
    specs_dir: Option<String>,
    tenant: String,
) -> Result<()> {
    // Initialize OTEL tracing if OTLP_ENDPOINT is set.
    // The guard must be held alive for the server's lifetime.
    let _otel_guard = std::env::var("OTLP_ENDPOINT").ok().map(|endpoint| {
        let service = if production {
            "temper-platform-prod"
        } else {
            "temper-platform-dev"
        };
        init_tracing(&endpoint, service).expect("Failed to initialize OTEL tracing")
    });

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    let state = if production {
        // Production mode: load specs from directory
        let specs_path = specs_dir
            .as_deref()
            .unwrap_or("specs");
        let registry = load_registry(specs_path, &tenant)?;
        PlatformState::new_production(registry, api_key)
    } else {
        // Dev mode (default): empty registry, start interview
        PlatformState::new_dev(api_key)
    };

    let mode_str = if production { "production" } else { "developer" };

    println!("Starting Temper platform server ({mode_str} mode)...");
    println!();
    println!("  Developer Studio: http://localhost:{port}/dev");
    println!("  Production Chat:  http://localhost:{port}/prod");
    println!("  OData API:        http://localhost:{port}/odata");
    println!();

    if dev || !production {
        println!("  Open the Developer Studio to start designing your application.");
    }
    if production {
        println!("  Tenant: {tenant}");
        if let Some(ref dir) = specs_dir {
            println!("  Specs:  {dir}");
        }
    }
    println!();

    let router = build_platform_router(state);
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .with_context(|| format!("Failed to bind to port {port}"))?;

    println!("Listening on http://0.0.0.0:{port}");
    axum::serve(listener, router)
        .await
        .context("Server error")?;

    Ok(())
}

/// Load specs from a directory into a SpecRegistry.
fn load_registry(specs_dir: &str, tenant: &str) -> Result<SpecRegistry> {
    let specs_path = Path::new(specs_dir);

    if !specs_path.is_dir() {
        anyhow::bail!("Specs directory not found: {}", specs_path.display());
    }

    // Read CSDL model
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        anyhow::bail!(
            "CSDL model not found at {}. Run `temper init` first.",
            csdl_path.display()
        );
    }

    let csdl_xml = fs::read_to_string(&csdl_path)
        .with_context(|| format!("Failed to read {}", csdl_path.display()))?;
    let csdl = parse_csdl(&csdl_xml)
        .with_context(|| format!("Failed to parse CSDL from {}", csdl_path.display()))?;

    // Read IOA TOML specs
    let ioa_sources = read_ioa_sources(specs_path)?;

    let mut registry = SpecRegistry::new();
    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    registry.register_tenant(tenant, csdl, csdl_xml, &ioa_pairs);

    Ok(registry)
}

/// Read all `.ioa.toml` files from the specs directory.
fn read_ioa_sources(specs_dir: &Path) -> Result<HashMap<String, String>> {
    let mut sources = HashMap::new();

    for entry in fs::read_dir(specs_dir)
        .with_context(|| format!("Failed to read specs directory: {}", specs_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        if file_name.ends_with(".ioa.toml") {
            let entity_name = file_name
                .strip_suffix(".ioa.toml")
                .unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);

            let source = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read IOA file: {}", path.display()))?;

            sources.insert(entity_name, source);
        }
    }

    Ok(sources)
}

/// Convert a string to PascalCase.
fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| c == '_' || c == '-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}
