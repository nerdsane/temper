//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.
//!
//! Specs are loaded immediately (design-time observation) and verification
//! runs in the background so the observe UI can stream progress.

mod loader;
mod storage;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;

use temper_evolution::PostgresRecordStore;
use temper_observe::ClickHouseStore;
use temper_observe::otel::init_tracing;
use temper_platform::optimization::run_optimization_cycle;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::event_store::ServerEventStore;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::state::DesignTimeEvent;
use temper_server::webhooks::WebhookDispatcher;
use temper_store_redis::RedisEventStore;
use temper_store_turso::TursoEventStore;
use temper_verify::cascade::VerificationCascade;

use crate::StorageBackend;

use loader::{hydrate_trajectory_log, load_into_registry, read_ioa_sources};
use storage::{
    connect_postgres_store, load_registry_from_postgres, load_registry_from_turso,
    redact_connection_url, upsert_loaded_specs_to_postgres, upsert_loaded_specs_to_turso,
};

/// Parsed specs loaded from disk for a tenant.
struct LoadedTenantSpecs {
    pub csdl_xml: String,
    pub ioa_sources: HashMap<String, String>,
    pub cross_invariants_toml: Option<String>,
}

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
    storage: StorageBackend,
    storage_explicit: bool,
    observe: bool,
) -> Result<()> {
    // Initialize OTEL tracing if OTLP_ENDPOINT is set.
    // The guard must be held alive for the server's lifetime.
    let _otel_guard = std::env::var("OTLP_ENDPOINT").ok().map(|endpoint| {
        init_tracing(&endpoint, "temper-platform").expect("Failed to initialize OTEL tracing")
    });

    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Select and initialize storage backend.
    let mut pg_pool: Option<sqlx::PgPool> = None;
    let event_store: Option<ServerEventStore> = match storage {
        StorageBackend::Postgres => {
            if let Ok(database_url) = std::env::var("DATABASE_URL") {
                let (store, pool) = connect_postgres_store(&database_url).await?;
                println!(
                    "  Storage: postgres ({})",
                    redact_connection_url(&database_url)
                );
                pg_pool = Some(pool);
                Some(store)
            } else if storage_explicit {
                anyhow::bail!("DATABASE_URL is required when --storage postgres is selected");
            } else {
                println!("  Storage: memory (in-memory only)");
                println!("  No DATABASE_URL — running in-memory only (state not persisted).");
                None
            }
        }
        StorageBackend::Turso => {
            let turso_url = match std::env::var("TURSO_URL") {
                Ok(url) => url,
                Err(_) => {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    let db_path = Path::new(&home).join(".local/share/temper/agents.db");
                    let parent_dir = db_path.parent().context(
                        "Failed to determine parent directory for default Turso DB path",
                    )?;
                    fs::create_dir_all(parent_dir).with_context(|| {
                        format!(
                            "Failed to create default Turso DB directory: {}",
                            parent_dir.display()
                        )
                    })?;
                    format!("file:{}", db_path.display())
                }
            };
            let turso_token = std::env::var("TURSO_AUTH_TOKEN").ok();
            let store = TursoEventStore::new(&turso_url, turso_token.as_deref())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to Turso/libSQL: {e}"))?;
            println!("  Storage: turso ({})", turso_url);
            Some(ServerEventStore::Turso(store))
        }
        StorageBackend::Redis => {
            let redis_url = std::env::var("REDIS_URL")
                .context("REDIS_URL is required when --storage redis is selected")?;
            let store = RedisEventStore::new(&redis_url)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to Redis: {e}"))?;
            println!("  Storage: redis ({})", redact_connection_url(&redis_url));
            Some(ServerEventStore::Redis(store))
        }
    };

    // Build initial registry from Postgres or Turso (recovery), then override with disk apps.
    let mut registry = SpecRegistry::new();
    if let Some(pool) = pg_pool.as_ref() {
        let restored = load_registry_from_postgres(&mut registry, pool).await?;
        if restored > 0 {
            println!("  Restored {restored} specs from Postgres.");
        }
    } else if let Some(ServerEventStore::Turso(ref turso)) = event_store {
        let restored = load_registry_from_turso(&mut registry, turso).await?;
        if restored > 0 {
            println!("  Restored {restored} specs from Turso.");
        }
    }

    for (tenant, specs_dir) in &apps {
        println!("  Loading app: {tenant} from {specs_dir}");
        let loaded = load_into_registry(&mut registry, specs_dir, tenant)?;
        if let Some(pool) = pg_pool.as_ref() {
            upsert_loaded_specs_to_postgres(pool, tenant, &loaded).await?;
        } else if let Some(ServerEventStore::Turso(ref turso)) = event_store {
            upsert_loaded_specs_to_turso(turso, tenant, &loaded).await?;
        }
    }

    let mut state = PlatformState::with_registry(registry, api_key);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let data_dir = Path::new(&home).join(".local/share/temper");
    state.server.data_dir = data_dir.clone();

    let specs_registry_path = data_dir.join("specs-registry.json");
    let mut auto_reloaded = 0usize;
    if let Ok(content) = fs::read_to_string(&specs_registry_path)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(entries) = value.as_object()
    {
        for (tenant, specs_dir) in entries {
            let Some(specs_dir) = specs_dir.as_str() else {
                continue;
            };

            let loaded = {
                let mut guard = state.registry.write().unwrap(); // ci-ok: infallible lock
                load_into_registry(&mut guard, specs_dir, tenant)
            };

            match loaded {
                Ok(_) => {
                    auto_reloaded += 1;
                }
                Err(e) => {
                    eprintln!(
                        "  Warning: failed to auto-reload app {tenant} from {specs_dir}: {e}"
                    );
                }
            }
        }
    }
    println!(
        "  Auto-reloaded {auto_reloaded} specs entries from {}",
        specs_registry_path.display()
    );

    // Build reaction dispatcher from tenant reaction specs.
    state.server.rebuild_reaction_dispatcher();

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
                        Err(e) => {
                            eprintln!("  Warning: failed to parse webhooks.toml for {tenant}: {e}")
                        }
                    },
                    Err(e) => {
                        eprintln!("  Warning: failed to read webhooks.toml for {tenant}: {e}")
                    }
                }
            }
        }
        if !all_configs.is_empty() {
            state.server.webhook_dispatcher = Some(Arc::new(WebhookDispatcher::new(all_configs)));
        }
    }

    // Wire up persistence if available.
    if let Some(store) = event_store {
        if let Some(pool) = store.postgres_pool().cloned() {
            state.server.pg_record_store = Some(Arc::new(PostgresRecordStore::new(pool)));
        }
        state.server.event_store = Some(Arc::new(store));
    }

    // Hydrate entities from the event store for each app tenant.
    if state.server.event_store.is_some() {
        for (tenant, _dir) in &apps {
            let tenant_id = temper_runtime::tenant::TenantId::new(tenant.as_str());
            state.server.hydrate_from_store(&tenant_id).await;
        }
    }

    // Hydrate trajectory log from persistent backend (Postgres, Turso, or Redis).
    if let Some(ref store) = state.server.event_store {
        hydrate_trajectory_log(&state.server, store, &apps).await;
    }

    // Recover WASM modules from persistent backend (Postgres or Turso).
    if state.server.event_store.is_some() {
        match state.server.load_wasm_modules().await {
            Ok(count) if count > 0 => {
                println!("  Recovered {count} WASM modules from database.");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("  Warning: failed to recover WASM modules: {e}");
            }
        }

        // Recover recent WASM invocation history.
        match state.server.load_recent_wasm_invocations(500).await {
            Ok(count) if count > 0 => {
                println!("  Restored {count} WASM invocation entries from database.");
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("  Warning: failed to recover WASM invocations: {e}");
            }
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
    let actual_port = listener
        .local_addr()
        .context("Failed to get listener local address")?
        .port();

    // Optionally start the Observe UI (Next.js dev server).
    if observe {
        spawn_observe_ui(actual_port);
    }

    // Spawn background verification AFTER the server is listening,
    // so the observe UI can connect and stream results.
    for (tenant, dir) in &apps {
        spawn_background_verification(&state, dir, tenant).await;
    }
    spawn_optimization_loop(&state);

    println!("Listening on http://0.0.0.0:{actual_port}");
    axum::serve(listener, router)
        .await
        .context("Server error")?;

    Ok(())
}

fn spawn_optimization_loop(state: &PlatformState) {
    let Some(store_url) = std::env::var("TEMPER_OPTIMIZE_STORE_URL").ok() else {
        return;
    };
    let interval_secs = std::env::var("TEMPER_OPTIMIZE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300)
        .clamp(1, 86_400);

    let state = state.clone();
    tokio::spawn(async move {
        let store = ClickHouseStore::new(&store_url);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume immediate tick

        loop {
            ticker.tick().await;
            let _ = run_optimization_cycle(&store, &state).await;
        }
    });
}

/// Spawn the Observe UI (Next.js dev server) in the background.
///
/// Looks for the `observe/` directory relative to the binary or cwd.
/// Falls back gracefully if npm/node_modules are unavailable.
fn spawn_observe_ui(api_port: u16) {
    // Try to find the observe directory relative to the binary, then cwd.
    let observe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| {
            let d = p.parent()?.parent()?.parent()?.join("observe");
            if d.exists() { Some(d) } else { None }
        })
        .or_else(|| {
            let d = std::env::current_dir().ok()?.join("observe");
            if d.exists() { Some(d) } else { None }
        });

    let Some(observe_dir) = observe_dir else {
        eprintln!("  Warning: observe/ directory not found, skipping Observe UI");
        return;
    };

    if !observe_dir.join("node_modules").exists() {
        eprintln!(
            "  Warning: observe/node_modules not found. Run `npm install` in {} first.",
            observe_dir.display()
        );
        return;
    }

    // Use a deterministic port for the Observe UI (API port + 1).
    // Next.js respects the PORT env var.
    let observe_port = api_port.saturating_add(1);
    println!("  Observe UI: http://localhost:{observe_port}");
    println!();

    tokio::spawn(async move {
        let result = tokio::process::Command::new("npm")
            .arg("run")
            .arg("dev")
            .env("NEXT_PUBLIC_TEMPER_API_PORT", api_port.to_string())
            .env("PORT", observe_port.to_string())
            .current_dir(&observe_dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        match result {
            Ok(mut child) => {
                if let Some(stderr) = child.stderr.take() {
                    let mut lines = tokio::io::BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.contains("error") || line.contains("Error") {
                            eprintln!("  [observe] {line}");
                        }
                    }
                }
                let _ = child.wait().await;
            }
            Err(e) => {
                eprintln!("  Warning: failed to start Observe UI: {e}");
            }
        }
    });
}

fn is_ephemeral_metadata_error(err: &str) -> bool {
    err.contains("explicit ephemeral mode")
}

fn emit_ephemeral_info(message: &str) {
    use std::io::Write as _;

    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
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
                if is_ephemeral_metadata_error(&e) {
                    emit_ephemeral_info(&format!(
                        "Info: {tenant}/{entity} verification status is in-memory only: {e}"
                    ));
                } else {
                    eprintln!(
                        "Warning: failed to persist running verification status for {tenant}/{entity}: {e}"
                    );
                    return;
                }
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
                        if is_ephemeral_metadata_error(&e) {
                            emit_ephemeral_info(&format!(
                                "Info: {tenant}/{entity} final verification status is in-memory only: {e}"
                            ));
                        } else {
                            eprintln!(
                                "Warning: failed to persist final verification status for {tenant}/{entity}: {e}"
                            );
                            return;
                        }
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
                        if is_ephemeral_metadata_error(&persist_err) {
                            emit_ephemeral_info(&format!(
                                "Info: {tenant}/{entity_clone} failed verification status is in-memory only: {persist_err}"
                            ));
                        } else {
                            eprintln!(
                                "Warning: failed to persist failed verification status for {tenant}/{entity_clone}: {persist_err}"
                            );
                            return;
                        }
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
