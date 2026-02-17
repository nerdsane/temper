//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.
//!
//! Specs are loaded immediately (design-time observation) and verification
//! runs in the background so the observe UI can stream progress.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_observe::otel::init_tracing;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::state::DesignTimeEvent;
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use temper_verify::cascade::VerificationCascade;

/// Run the `temper serve` command.
///
/// Starts the Temper platform server. Specs are loaded immediately so the
/// observe UI can display state machines. Verification runs in the background
/// and results stream via SSE (design-time observation).
///
/// `apps` is a list of `(tenant_name, specs_dir)` pairs. Can be empty (no user apps).
pub async fn run(
    port: u16,
    apps: Vec<(String, String)>,
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

    // Load specs from all apps without blocking on verification.
    let mut state = if !apps.is_empty() {
        let mut registry = SpecRegistry::new();
        for (tenant, specs_dir) in &apps {
            println!("  Loading app: {tenant} from {specs_dir}");
            load_into_registry(&mut registry, specs_dir, tenant)?;
        }
        PlatformState::with_registry(registry, api_key)
    } else {
        PlatformState::new(api_key)
    };

    // Wire up Postgres persistence if available.
    if let Some(store) = event_store {
        state.server.event_store = Some(Arc::new(store));
    }

    println!("Starting Temper platform server...");
    println!();
    println!("  Temper Data API: http://localhost:{port}/tdata");
    println!();

    for (tenant, dir) in &apps {
        println!("  App: {tenant} ({dir})");
    }
    if !apps.is_empty() {
        println!("  Verification: running in background (observe UI will stream progress)");
        println!();
    }

    // Bootstrap the system tenant (Project, Tenant, CatalogEntry, etc.)
    temper_platform::bootstrap_system_tenant(&state);

    let router = build_platform_router(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .with_context(|| format!("Failed to bind to port {port}"))?;

    // Spawn background verification AFTER the server is listening,
    // so the observe UI can connect and stream results.
    for (tenant, dir) in &apps {
        spawn_background_verification(&state, dir, tenant);
    }

    println!("Listening on http://0.0.0.0:{port}");
    axum::serve(listener, router)
        .await
        .context("Server error")?;

    Ok(())
}

/// Load specs from a directory into an existing SpecRegistry WITHOUT running verification.
///
/// All entities start with `VerificationStatus::Pending`. The observe UI
/// can display state machines immediately while verification runs in background.
fn load_into_registry(registry: &mut SpecRegistry, specs_dir: &str, tenant: &str) -> Result<()> {
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

    for entity_name in ioa_sources.keys() {
        println!("    Loaded spec: {entity_name} (verification pending)");
    }

    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    registry.register_tenant(tenant, csdl, csdl_xml, &ioa_pairs);

    Ok(())
}

/// Spawn background verification tasks for each entity in the specs directory.
///
/// For each entity, runs the verification cascade in a blocking task and
/// updates the registry and broadcasts design-time events as progress is made.
/// Helper: send a design-time event to both the broadcast channel and the persistent log.
fn emit_design_time_event(
    tx: &tokio::sync::broadcast::Sender<DesignTimeEvent>,
    log: &std::sync::RwLock<Vec<DesignTimeEvent>>,
    event: DesignTimeEvent,
) {
    let _ = tx.send(event.clone());
    if let Ok(mut entries) = log.write() {
        if entries.len() < 10_000 {
            entries.push(event);
        }
    }
}

fn spawn_background_verification(state: &PlatformState, specs_dir: &str, tenant: &str) {
    let specs_path = Path::new(specs_dir);
    let ioa_sources = match read_ioa_sources(specs_path) {
        Ok(sources) => sources,
        Err(e) => {
            eprintln!("Warning: failed to read IOA sources for background verification: {e}");
            return;
        }
    };

    let registry = state.registry.clone();
    let design_time_tx = state.server.design_time_tx.clone();
    let design_time_log = state.server.design_time_log.clone();
    let tenant_str = tenant.to_string();

    // Emit spec_loaded events for each entity
    for entity_name in ioa_sources.keys() {
        emit_design_time_event(&design_time_tx, &design_time_log, DesignTimeEvent {
            kind: "spec_loaded".to_string(),
            entity_type: entity_name.clone(),
            tenant: tenant_str.clone(),
            summary: format!("Loaded spec: {entity_name}"),
            level: None,
            passed: None,
            timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
            step_number: Some(1),
            total_steps: Some(7),
        });
    }

    for (entity_name, ioa_source) in ioa_sources {
        let registry = registry.clone();
        let design_time_tx = design_time_tx.clone();
        let design_time_log = design_time_log.clone();
        let tenant = tenant_str.clone();
        let entity = entity_name.clone();

        tokio::spawn(async move {
            // Mark as Running
            {
                let tenant_id = temper_runtime::tenant::TenantId::new(&tenant);
                let mut reg = registry.write().unwrap();
                reg.set_verification_status(
                    &tenant_id,
                    &entity,
                    VerificationStatus::Running,
                );
            }

            emit_design_time_event(&design_time_tx, &design_time_log, DesignTimeEvent {
                kind: "verify_started".to_string(),
                entity_type: entity.clone(),
                tenant: tenant.clone(),
                summary: format!("Verification started for {entity}"),
                level: None,
                passed: None,
                timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                step_number: Some(2),
                total_steps: Some(7),
            });

            println!("  [verify] Starting verification for {entity}...");

            // Run the cascade in a blocking task (CPU-intensive).
            let entity_clone = entity.clone();
            let result = tokio::task::spawn_blocking(move || {
                VerificationCascade::from_ioa(&ioa_source)
                    .with_sim_seeds(5)
                    .with_prop_test_cases(100)
                    .run()
            })
            .await;

            match result {
                Ok(cascade_result) => {
                    // Send per-level events
                    for (i, level) in cascade_result.levels.iter().enumerate() {
                        let status_str = if level.passed { "PASS" } else { "FAIL" };
                        println!("  [verify] {entity}: [{status_str}] {}", level.summary);

                        emit_design_time_event(&design_time_tx, &design_time_log, DesignTimeEvent {
                            kind: "verify_level".to_string(),
                            entity_type: entity.clone(),
                            tenant: tenant.clone(),
                            summary: level.summary.clone(),
                            level: Some(format!("{:?}", level.level)),
                            passed: Some(level.passed),
                            timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                            step_number: Some(3 + i as u8), // L0=3, L1=4, L2=5, L3=6
                            total_steps: Some(7),
                        });
                    }

                    // Build verification result
                    let verification_result = EntityVerificationResult {
                        all_passed: cascade_result.all_passed,
                        levels: cascade_result
                            .levels
                            .iter()
                            .map(|l| EntityLevelSummary {
                                level: format!("{:?}", l.level),
                                passed: l.passed,
                                summary: l.summary.clone(),
                            })
                            .collect(),
                        verified_at: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                    };

                    let all_passed = cascade_result.all_passed;

                    // Update registry
                    {
                        let tenant_id = temper_runtime::tenant::TenantId::new(&tenant);
                        let mut reg = registry.write().unwrap();
                        reg.set_verification_status(
                            &tenant_id,
                            &entity,
                            VerificationStatus::Completed(verification_result),
                        );
                    }

                    let summary = if all_passed {
                        format!("{entity}: all levels passed")
                    } else {
                        format!("{entity}: some levels failed")
                    };
                    println!("  [verify] {summary}");

                    emit_design_time_event(&design_time_tx, &design_time_log, DesignTimeEvent {
                        kind: "verify_done".to_string(),
                        entity_type: entity,
                        tenant,
                        summary,
                        level: None,
                        passed: Some(all_passed),
                        timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                        step_number: Some(7),
                        total_steps: Some(7),
                    });
                }
                Err(e) => {
                    eprintln!("  [verify] {entity_clone}: verification task panicked: {e}");

                    let verification_result = EntityVerificationResult {
                        all_passed: false,
                        levels: vec![EntityLevelSummary {
                            level: "Error".to_string(),
                            passed: false,
                            summary: format!("Verification task panicked: {e}"),
                        }],
                        verified_at: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                    };

                    {
                        let tenant_id = temper_runtime::tenant::TenantId::new(&tenant);
                        let mut reg = registry.write().unwrap();
                        reg.set_verification_status(
                            &tenant_id,
                            &entity_clone,
                            VerificationStatus::Completed(verification_result),
                        );
                    }

                    emit_design_time_event(&design_time_tx, &design_time_log, DesignTimeEvent {
                        kind: "verify_done".to_string(),
                        entity_type: entity_clone,
                        tenant,
                        summary: "Verification panicked".to_string(),
                        level: None,
                        passed: Some(false),
                        timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                        step_number: Some(7),
                        total_steps: Some(7),
                    });
                }
            }
        });
    }
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
