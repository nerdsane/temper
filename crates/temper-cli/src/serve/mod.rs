//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use temper_observe::otel::init_tracing;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use temper_verify::cascade::VerificationCascade;

/// Run the `temper serve` command.
///
/// Starts the Temper platform server. Both build mode (accepting new specs)
/// and use mode (serving deployed entities) can be active simultaneously.
/// If `DATABASE_URL` is set, events are persisted to Postgres.
pub async fn run(
    port: u16,
    specs_dir: Option<String>,
    tenant: String,
) -> Result<()> {
    // Initialize OTEL tracing if OTLP_ENDPOINT is set.
    // The guard must be held alive for the server's lifetime.
    let _otel_guard = std::env::var("OTLP_ENDPOINT").ok().map(|endpoint| {
        init_tracing(&endpoint, "temper-platform").expect("Failed to initialize OTEL tracing")
    });

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Connect to Postgres if DATABASE_URL is set.
    let event_store = if let Ok(database_url) = std::env::var("DATABASE_URL") {
        println!("  Connecting to Postgres...");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .context("Failed to connect to Postgres")?;
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to run migrations: {e}"))?;
        println!("  Postgres connected, migrations applied.");
        Some(PostgresEventStore::new(pool))
    } else {
        println!("  No DATABASE_URL — running in-memory only (events not persisted).");
        None
    };

    let mut state = if let Some(ref specs_path) = specs_dir {
        let registry = load_registry(specs_path, &tenant)?;
        PlatformState::with_registry(registry, api_key)
    } else {
        PlatformState::new(api_key)
    };

    // Wire up Postgres persistence if available.
    if let Some(store) = event_store {
        use std::sync::Arc;
        state.server.event_store = Some(Arc::new(store));
    }

    println!("Starting Temper platform server...");
    println!();
    println!("  Temper Data API: http://localhost:{port}/tdata");
    println!();

    if let Some(ref dir) = specs_dir {
        println!("  Tenant: {tenant}");
        println!("  Specs:  {dir}");
        println!();
    }

    // Bootstrap the system tenant (Project, Tenant, CatalogEntry, etc.)
    temper_platform::bootstrap_system_tenant(&state);

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

    // Verify each IOA spec before registration
    for (entity_name, ioa_source) in &ioa_sources {
        println!("  Verifying {entity_name}...");
        let cascade = VerificationCascade::from_ioa(ioa_source)
            .with_sim_seeds(5)
            .with_prop_test_cases(100);
        let result = cascade.run();
        for level in &result.levels {
            let status = if level.passed { "PASS" } else { "FAIL" };
            println!("    [{status}] {}", level.summary);
        }
        if !result.all_passed {
            anyhow::bail!("Verification failed for entity '{entity_name}'");
        }
    }

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
    s.split(['_', '-'])
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
