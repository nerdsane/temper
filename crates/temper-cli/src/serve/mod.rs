//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.
//!
//! Specs are loaded immediately (design-time observation) and verification
//! runs in the background so the observe UI can stream progress.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use temper_evolution::PostgresRecordStore;
use temper_observe::otel::init_tracing;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::state::DesignTimeEvent;
use temper_server::webhooks::WebhookDispatcher;
use temper_spec::csdl::parse_csdl;
use temper_store_postgres::PostgresEventStore;
use temper_verify::cascade::VerificationCascade;

/// Parsed specs loaded from disk for a tenant.
struct LoadedTenantSpecs {
    csdl_xml: String,
    ioa_sources: HashMap<String, String>,
}

#[derive(sqlx::FromRow)]
struct PersistedSpecRow {
    tenant: String,
    entity_type: String,
    ioa_source: String,
    csdl_xml: Option<String>,
    verification_status: String,
    verified: bool,
    levels_passed: Option<i32>,
    levels_total: Option<i32>,
    verification_result: Option<serde_json::Value>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// Run the `temper serve` command.
///
/// Starts the Temper platform server. Specs are loaded immediately so the
/// observe UI can display state machines. Verification runs in the background
/// and results stream via SSE (design-time observation).
///
/// `apps` is a list of `(tenant_name, specs_dir)` pairs. Can be empty (no user apps).
pub async fn run(port: u16, apps: Vec<(String, String)>) -> Result<()> {
    // Initialize OTEL tracing if OTLP_ENDPOINT is set.
    // The guard must be held alive for the server's lifetime.
    let _otel_guard = std::env::var("OTLP_ENDPOINT").ok().map(|endpoint| {
        init_tracing(&endpoint, "temper-platform").expect("Failed to initialize OTEL tracing")
    });

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Connect to Postgres if DATABASE_URL is set.
    let mut pg_pool: Option<sqlx::PgPool> = None;
    let event_store = if let Ok(database_url) = std::env::var("DATABASE_URL") {
        println!("  Connecting to Postgres...");
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .context("Failed to connect to Postgres")?;
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to run migrations: {e}"))?;
        let pg_record_store: PostgresRecordStore = PostgresRecordStore::new(pool.clone());
        pg_record_store
            .migrate()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to migrate evolution_records: {e}"))?;
        println!("  Postgres connected, migrations applied.");
        pg_pool = Some(pool.clone());
        Some(PostgresEventStore::new(pool))
    } else {
        println!("  No DATABASE_URL — running in-memory only (state not persisted).");
        None
    };

    // Build initial registry from Postgres (recovery), then override with disk apps if provided.
    let mut registry = SpecRegistry::new();
    if let Some(pool) = pg_pool.as_ref() {
        let restored = load_registry_from_postgres(&mut registry, pool).await?;
        if restored > 0 {
            println!("  Restored {restored} specs from Postgres.");
        }
    }

    for (tenant, specs_dir) in &apps {
        println!("  Loading app: {tenant} from {specs_dir}");
        let loaded = load_into_registry(&mut registry, specs_dir, tenant)?;
        if let Some(pool) = pg_pool.as_ref() {
            upsert_loaded_specs_to_postgres(pool, tenant, &loaded).await?;
        }
    }

    let mut state = PlatformState::with_registry(registry, api_key);

    // Load webhooks.toml from each app directory and build a merged dispatcher.
    {
        let mut all_configs = Vec::new();
        for (tenant, specs_dir) in &apps {
            let path = Path::new(specs_dir).join("webhooks.toml");
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(source) => match WebhookDispatcher::from_toml(&source) {
                        Ok(d) => {
                            println!("  Loaded webhooks.toml for {tenant}");
                            all_configs.extend(d.configs().iter().cloned());
                        }
                        Err(e) => eprintln!("  Warning: failed to parse webhooks.toml for {tenant}: {e}"),
                    },
                    Err(e) => eprintln!("  Warning: failed to read webhooks.toml for {tenant}: {e}"),
                }
            }
        }
        if !all_configs.is_empty() {
            state.server.webhook_dispatcher = Some(Arc::new(WebhookDispatcher::new(all_configs)));
        }
    }

    // Wire up Postgres persistence if available.
    if let Some(store) = event_store {
        let pool = store.pool().clone();
        state.server.event_store = Some(Arc::new(store));
        state.server.pg_record_store = Some(Arc::new(PostgresRecordStore::new(pool)));
    }

    // Hydrate entities from the event store for each app tenant.
    if state.server.event_store.is_some() {
        for (tenant, _dir) in &apps {
            let tenant_id = temper_runtime::tenant::TenantId::new(tenant.as_str());
            state.server.hydrate_from_store(&tenant_id).await;
        }
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
        spawn_background_verification(&state, dir, tenant).await;
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
fn load_into_registry(
    registry: &mut SpecRegistry,
    specs_dir: &str,
    tenant: &str,
) -> Result<LoadedTenantSpecs> {
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

    Ok(LoadedTenantSpecs {
        csdl_xml: registry
            .get_tenant(&TenantId::new(tenant))
            .map(|cfg| cfg.csdl_xml.as_ref().clone())
            .unwrap_or_default(),
        ioa_sources,
    })
}

async fn upsert_loaded_specs_to_postgres(
    pool: &sqlx::PgPool,
    tenant: &str,
    loaded: &LoadedTenantSpecs,
) -> Result<()> {
    for (entity_type, ioa_source) in &loaded.ioa_sources {
        sqlx::query(
            "INSERT INTO specs \
             (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at) \
             VALUES ($1, $2, $3, $4, 1, false, 'pending', now()) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, \
                 csdl_xml = EXCLUDED.csdl_xml, \
                 version = specs.version + 1, \
                 verified = false, \
                 verification_status = 'pending', \
                 levels_passed = NULL, \
                 levels_total = NULL, \
                 verification_result = NULL, \
                 updated_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(ioa_source)
        .bind(&loaded.csdl_xml)
        .execute(pool)
        .await
        .with_context(|| format!("Failed to persist spec {tenant}/{entity_type}"))?;
    }
    Ok(())
}

fn persisted_status_to_registry_status(row: &PersistedSpecRow) -> VerificationStatus {
    let status = row.verification_status.to_lowercase();
    match status.as_str() {
        "pending" => VerificationStatus::Pending,
        "running" => VerificationStatus::Running,
        _ => {
            if let Some(value) = row.verification_result.clone() {
                if let Ok(result) = serde_json::from_value::<EntityVerificationResult>(value) {
                    return VerificationStatus::Completed(result);
                }
            }

            let all_passed = status == "passed" || row.verified;
            let levels_passed = row
                .levels_passed
                .unwrap_or(if all_passed { 1 } else { 0 })
                .max(0) as usize;
            let levels_total = row.levels_total.unwrap_or(levels_passed as i32).max(0) as usize;
            let levels = if levels_total > 0 {
                (0..levels_total)
                    .map(|idx| EntityLevelSummary {
                        level: format!("L{idx}"),
                        passed: idx < levels_passed,
                        summary: if idx < levels_passed {
                            "Restored from persisted verification summary".to_string()
                        } else {
                            "Restored failed verification level".to_string()
                        },
                        details: None,
                    })
                    .collect()
            } else {
                vec![EntityLevelSummary {
                    level: "Persisted".to_string(),
                    passed: all_passed,
                    summary: format!("Restored status '{}'", row.verification_status),
                    details: None,
                }]
            };
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed,
                levels,
                verified_at: row.updated_at.to_rfc3339(),
            })
        }
    }
}

async fn load_registry_from_postgres(
    registry: &mut SpecRegistry,
    pool: &sqlx::PgPool,
) -> Result<usize> {
    let rows: Vec<PersistedSpecRow> = sqlx::query_as(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                levels_passed, levels_total, verification_result, updated_at \
         FROM specs \
         ORDER BY tenant, entity_type",
    )
    .fetch_all(pool)
    .await
    .context("Failed to read specs from Postgres")?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut grouped: BTreeMap<String, Vec<PersistedSpecRow>> = BTreeMap::new();
    for row in rows {
        grouped.entry(row.tenant.clone()).or_default().push(row);
    }

    let mut restored_specs = 0usize;
    for (tenant, tenant_rows) in grouped {
        let csdl_xml = tenant_rows
            .iter()
            .find_map(|row| row.csdl_xml.clone())
            .unwrap_or_default();
        if csdl_xml.trim().is_empty() {
            eprintln!("Warning: skipping restored tenant '{tenant}' due to missing CSDL");
            continue;
        }
        let csdl = parse_csdl(&csdl_xml)
            .with_context(|| format!("Failed to parse restored CSDL for tenant '{tenant}'"))?;

        let ioa_owned: Vec<(String, String)> = tenant_rows
            .iter()
            .map(|row| (row.entity_type.clone(), row.ioa_source.clone()))
            .collect();
        let ioa_pairs: Vec<(&str, &str)> = ioa_owned
            .iter()
            .map(|(entity_type, ioa)| (entity_type.as_str(), ioa.as_str()))
            .collect();

        registry.register_tenant(tenant.as_str(), csdl, csdl_xml, &ioa_pairs);
        let tenant_id = TenantId::new(&tenant);
        for row in &tenant_rows {
            registry.set_verification_status(
                &tenant_id,
                &row.entity_type,
                persisted_status_to_registry_status(row),
            );
            restored_specs += 1;
        }
    }

    Ok(restored_specs)
}

/// Spawn background verification tasks for each entity in the specs directory.
///
/// For each entity, runs the verification cascade in a blocking task and
/// updates the registry while persisting workflow history/status to Postgres.
async fn spawn_background_verification(state: &PlatformState, specs_dir: &str, tenant: &str) {
    let specs_path = Path::new(specs_dir);
    let ioa_sources = match read_ioa_sources(specs_path) {
        Ok(sources) => sources,
        Err(e) => {
            eprintln!("Warning: failed to read IOA sources for background verification: {e}");
            return;
        }
    };

    let registry = state.registry.clone();
    let server = state.server.clone();
    let tenant_str = tenant.to_string();

    // Emit spec_loaded events for each entity
    for entity_name in ioa_sources.keys() {
        if let Err(e) = server
            .emit_design_time_event(DesignTimeEvent {
                kind: "spec_loaded".to_string(),
                entity_type: entity_name.clone(),
                tenant: tenant_str.clone(),
                summary: format!("Loaded spec: {entity_name}"),
                level: None,
                passed: None,
                timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                step_number: Some(1),
                total_steps: Some(7),
            })
            .await
        {
            eprintln!(
                "Warning: failed to persist/emit spec_loaded for {tenant_str}/{entity_name}: {e}"
            );
        }
    }

    for (entity_name, ioa_source) in ioa_sources {
        let registry = registry.clone();
        let server = server.clone();
        let tenant = tenant_str.clone();
        let entity = entity_name.clone();

        tokio::spawn(async move {
            // Persist running status first, then update in-memory registry.
            if let Err(e) = server
                .persist_spec_verification(&tenant, &entity, "running", None)
                .await
            {
                eprintln!(
                    "Warning: failed to persist running verification status for {tenant}/{entity}: {e}"
                );
                return;
            }
            {
                let tenant_id = TenantId::new(&tenant);
                let mut reg = registry.write().unwrap();
                reg.set_verification_status(&tenant_id, &entity, VerificationStatus::Running);
            }

            if let Err(e) = server
                .emit_design_time_event(DesignTimeEvent {
                    kind: "verify_started".to_string(),
                    entity_type: entity.clone(),
                    tenant: tenant.clone(),
                    summary: format!("Verification started for {entity}"),
                    level: None,
                    passed: None,
                    timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                    step_number: Some(2),
                    total_steps: Some(7),
                })
                .await
            {
                eprintln!(
                    "Warning: failed to persist/emit verify_started for {tenant}/{entity}: {e}"
                );
                return;
            }

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

                        if let Err(e) = server
                            .emit_design_time_event(DesignTimeEvent {
                                kind: "verify_level".to_string(),
                                entity_type: entity.clone(),
                                tenant: tenant.clone(),
                                summary: level.summary.clone(),
                                level: Some(format!("{:?}", level.level)),
                                passed: Some(level.passed),
                                timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                                step_number: Some(3 + i as u8), // L0=3, L1=4, L2=5, L3=6
                                total_steps: Some(7),
                            })
                            .await
                        {
                            eprintln!(
                                "Warning: failed to persist/emit verify_level for {tenant}/{entity}: {e}"
                            );
                        }
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
                                details: None,
                            })
                            .collect(),
                        verified_at: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                    };

                    let all_passed = cascade_result.all_passed;

                    let passed_count = verification_result
                        .levels
                        .iter()
                        .filter(|l| l.passed)
                        .count();
                    let final_status = if verification_result.all_passed {
                        "passed"
                    } else if passed_count == 0 {
                        "failed"
                    } else {
                        "partial"
                    };
                    if let Err(e) = server
                        .persist_spec_verification(
                            &tenant,
                            &entity,
                            final_status,
                            Some(&verification_result),
                        )
                        .await
                    {
                        eprintln!(
                            "Warning: failed to persist final verification status for {tenant}/{entity}: {e}"
                        );
                        return;
                    }
                    {
                        let tenant_id = TenantId::new(&tenant);
                        let mut reg = registry.write().unwrap();
                        reg.set_verification_status(
                            &tenant_id,
                            &entity,
                            VerificationStatus::Completed(verification_result.clone()),
                        );
                    }

                    let summary = if all_passed {
                        format!("{entity}: all levels passed")
                    } else {
                        format!("{entity}: some levels failed")
                    };
                    println!("  [verify] {summary}");

                    if let Err(e) = server
                        .emit_design_time_event(DesignTimeEvent {
                            kind: "verify_done".to_string(),
                            entity_type: entity,
                            tenant: tenant.clone(),
                            summary,
                            level: None,
                            passed: Some(all_passed),
                            timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                            step_number: Some(7),
                            total_steps: Some(7),
                        })
                        .await
                    {
                        eprintln!("Warning: failed to persist/emit verify_done for {tenant}: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("  [verify] {entity_clone}: verification task panicked: {e}");

                    let verification_result = EntityVerificationResult {
                        all_passed: false,
                        levels: vec![EntityLevelSummary {
                            level: "Error".to_string(),
                            passed: false,
                            summary: format!("Verification task panicked: {e}"),
                            details: None,
                        }],
                        verified_at: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                    };

                    if let Err(persist_err) = server
                        .persist_spec_verification(
                            &tenant,
                            &entity_clone,
                            "failed",
                            Some(&verification_result),
                        )
                        .await
                    {
                        eprintln!(
                            "Warning: failed to persist failed verification status for {tenant}/{entity_clone}: {persist_err}"
                        );
                        return;
                    }
                    {
                        let tenant_id = TenantId::new(&tenant);
                        let mut reg = registry.write().unwrap();
                        reg.set_verification_status(
                            &tenant_id,
                            &entity_clone,
                            VerificationStatus::Completed(verification_result.clone()),
                        );
                    }
                    if let Err(event_err) = server
                        .emit_design_time_event(DesignTimeEvent {
                            kind: "verify_done".to_string(),
                            entity_type: entity_clone,
                            tenant: tenant.clone(),
                            summary: "Verification panicked".to_string(),
                            level: None,
                            passed: Some(false),
                            timestamp: chrono::Utc::now().to_rfc3339(), // determinism-ok: CLI code
                            step_number: Some(7),
                            total_steps: Some(7),
                        })
                        .await
                    {
                        eprintln!(
                            "Warning: failed to persist/emit verify_done panic event for {tenant}: {event_err}"
                        );
                    }
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
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
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
