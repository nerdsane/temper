//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.
//!
//! Specs are loaded immediately (design-time observation) and verification
//! runs in the background so the observe UI can stream progress.

mod actor_runtime;
mod boot;
mod bootstrap;
mod data_dir;
mod installed_apps;
mod later;
mod loader;
mod local_sandbox;
mod observe_ui;
pub(crate) mod options;
#[cfg(test)]
mod pushed_boot_tests;
mod restore;
#[cfg(test)]
mod serve_first_tests;
mod storage;
mod sync_copy;
#[cfg(test)]
mod unavailable_tenant_tests;

use std::collections::{BTreeMap, HashMap};
use std::future::IntoFuture;
use std::net::{Ipv4Addr, SocketAddr};
use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};

use temper_evolution::PostgresRecordStore;
use temper_observe::ClickHouseStore;
use temper_observe::otel::init_observability;
use temper_platform::optimization::run_optimization_cycle;
use temper_platform::router::build_platform_routes;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::state::DesignTimeEvent;
use temper_verify::cascade::VerificationCascade;

use crate::{ActorRuntimeBackend, StorageBackend};

use loader::read_ioa_sources;

/// Parsed specs loaded from disk for a tenant.
struct LoadedTenantSpecs {
    pub csdl_xml: String,
    pub ioa_sources: HashMap<String, String>,
    pub cross_invariants_toml: Option<String>,
    pub cedar_policy_text: Option<String>,
}

/// Run the `temper serve` command.
///
/// Starts the Temper platform server. Specs are loaded immediately so the
/// observe UI can display state machines. Verification runs in the background
/// and results stream via SSE (design-time observation).
///
/// `apps` is a list of `(tenant_name, specs_dir)` pairs. Can be empty (no user apps).
///
/// Startup is split into explicit phases (see [`bootstrap`] module):
/// 1. Storage init  2. Registry build  3. Auto-reload  4. Webhooks
/// 5. Persistence wiring  6. Entity hydration  7. Policy/WASM recovery
/// 8. Tenant bootstrap  9. Server start
#[allow(clippy::too_many_arguments)]
pub async fn run(
    bind: Ipv4Addr,
    port: u16,
    apps: Vec<(String, String)>,
    os_app_installs: Vec<String>,
    storage: StorageBackend,
    storage_explicit: bool,
    actor_runtime: ActorRuntimeBackend,
    actor_backed_type: Vec<String>,
    observe: bool,
    verify_subprocess: bool,
    discord_bot_token: Option<String>,
    tenant: String,
) -> Result<()> {
    let otel_guard = init_observability("temper-platform");
    temper_authz::init_metrics();
    temper_store_postgres::init_metrics();
    temper_store_turso::init_metrics();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Phase 1: Storage backend
    let (pg_pool, storage_stack, turso_synced_store) =
        bootstrap::init_storage(storage.clone(), storage_explicit).await?;

    // Phase 2: Registry (restore + disk apps)
    let boot::Stored {
        registry,
        policy_seed: tenant_policy_seed,
        mut later,
    } = boot::read_stored(pg_pool.as_ref(), storage_stack.as_ref(), &apps).await?;
    // build_registry already said which disk apps it skipped; the later phases
    // (webhooks, hydration, tenant bootstrap, verification) skip them as well.
    let apps: Vec<(String, String)> = apps
        .into_iter()
        .filter(|(tenant, _)| {
            registry
                .tenant_unavailable_reason(&temper_runtime::tenant::TenantId::new(tenant))
                .is_none()
        })
        .collect();

    // Assemble platform state
    let mut state = PlatformState::with_registry(registry, api_key);
    let mut pg_actor_runtime_cancel = None;
    state.api_token = std::env::var("TEMPER_API_KEY").ok();
    if state.api_token.is_some() {
        println!("  API key: configured (Bearer token required)");
    }
    // Jev reads an app's changed modules at install when this server is given its URL and a key or pass; never printed.
    if let Some(judge) = temper_platform::module_review::JevJudge::from_env() {
        state.set_module_reviewer(std::sync::Arc::new(
            temper_platform::module_review::ModuleReviewer::new(std::sync::Arc::new(judge)),
        ));
    }
    // TEMPER_DATA_DIR puts this server's files (pushed app versions, blobs,
    // caches) where they outlive the machine, such as a mounted volume.
    state.server.data_dir = data_dir::data_dir();
    state.server.files_need_object_store = storage::files_need_object_store();

    // Configure subprocess verification if requested.
    if verify_subprocess {
        let bin = std::env::current_exe() // determinism-ok: read once at startup
            .unwrap_or_else(|_| std::path::PathBuf::from("temper"));
        state.server.verify_subprocess_bin = Some(Arc::new(bin));
        println!("  Verification mode: subprocess (30s timeout per entity)");
    }

    // Phase 4: Webhooks
    state.server.webhook_dispatcher = bootstrap::load_webhooks(&apps);

    // Phase 5: Persistence wiring. The size limits are read even without a store,
    // so a bad value stops every start, not only a persisted one.
    let limits = storage::event_size_limits(
        std::env::var("TEMPER_WARN_EVENT_BYTES").ok().as_deref(),
        std::env::var("TEMPER_MAX_EVENT_BYTES").ok().as_deref(),
    )?;
    if let Some(stack) = storage_stack {
        if let Some(pool) = stack.postgres_pool.clone() {
            state.server.pg_record_store = Some(Arc::new(PostgresRecordStore::new(pool)));
        }
        state
            .server
            .set_storage_stack(stack.with_event_size_limits(limits));
    }

    // Phase 5b: Secrets vault
    {
        use base64::Engine as _;
        let key_bytes: [u8; 32] = if let Ok(key_b64) = std::env::var("TEMPER_VAULT_KEY") {
            // determinism-ok: read once at startup
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&key_b64)
                .expect("TEMPER_VAULT_KEY must be valid base64");
            assert_eq!(decoded.len(), 32, "TEMPER_VAULT_KEY must be 32 bytes");
            decoded.try_into().unwrap() // ci-ok: length asserted == 32 above
        } else {
            // No explicit key — generate an ephemeral one for in-memory secret caching.
            // determinism-ok: OsRng used once at startup for vault key generation
            use rand::RngCore as _;
            let mut key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut key);
            key
        };
        let vault = temper_server::secrets::vault::SecretsVault::new(&key_bytes);
        state.server.secrets_vault = Some(std::sync::Arc::new(vault));
        println!("  Secrets vault: configured");
    }

    // Seed secrets from env into the vault for all tenants.
    if let Some(ref vault) = state.server.secrets_vault {
        // ANTHROPIC_API_KEY — makes {secret:anthropic_api_key} resolve in LLM integrations.
        cache_platform_secret_if_present(
            vault,
            "anthropic_api_key",
            std::env::var("ANTHROPIC_API_KEY").ok(),
        );

        // EXA_API_KEY — makes {secret:exa_api_key} resolve in research integrations.
        cache_platform_secret_if_present(vault, "exa_api_key", std::env::var("EXA_API_KEY").ok());

        // blob_endpoint — points blob_adapter at the server's internal blob storage
        // when no external blob endpoint (R2/S3) is configured.
        // determinism-ok: env var read at startup for configuration
        if std::env::var("BLOB_ENDPOINT").is_err() {
            let blob_url = format!("http://127.0.0.1:{port}/_internal/blobs");
            vault.cache_platform_secret("blob_endpoint", blob_url);
        }

        // temper_api_url — points WASM modules at this server for TemperFS calls.
        {
            let api_url = format!("http://127.0.0.1:{port}");
            vault.cache_platform_secret("temper_api_url", api_url);
        }

        // sandbox_url — local sandbox for tool execution.
        // Uses SANDBOX_URL env var if set, otherwise auto-starts local_sandbox.py.
        // determinism-ok: env var read at startup for configuration
        {
            let sandbox_url = if let Ok(url) = std::env::var("SANDBOX_URL") {
                println!("  Sandbox: {url} (from SANDBOX_URL)");
                url
            } else {
                let sandbox_port = port + 10; // e.g., 3000 → 3010
                let sandbox_url = format!("http://127.0.0.1:{sandbox_port}");
                local_sandbox::start(&state.server, sandbox_port, &sandbox_url);

                sandbox_url
            };
            vault.cache_platform_secret("sandbox_url", sandbox_url);
        }

        // TEMPER_SECRET_<NAME> — any other secret the operator supplies. Runs
        // last, so every name seeded above keeps its value.
        // determinism-ok: environment read once at startup
        temper_server::secrets::seed_platform_secrets_from_environment(vault, std::env::vars_os());
    }

    // Startup banner
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

    // Phases 6-8b: the server's own tenants: entities, Cedar, WASM, secrets,
    // system and agent specs, installed apps and CLI `--app` requests.
    boot::restore_own(
        &state,
        tenant_policy_seed,
        &apps,
        &os_app_installs,
        &mut later,
    )
    .await?;
    if actor_runtime == ActorRuntimeBackend::Postgres {
        pg_actor_runtime_cancel = Some(
            actor_runtime::install_postgres_actor_runtime(
                storage,
                pg_pool.is_some(),
                &actor_backed_type,
                &mut state.server,
            )
            .await?,
        );
    }

    // Phase 9: Bind, start background tasks, serve
    let router = sync_copy::serve_router(build_platform_routes(state.clone()), turso_synced_store);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((bind, port)))
        .await
        .with_context(|| format!("Failed to bind to {bind}:{port}"))?;
    let actual_port = listener
        .local_addr()
        .context("Failed to get listener local address")?
        .port();
    let _ = state.server.listen_port.set(actual_port);

    if observe {
        observe_ui::spawn(bind, actual_port);
    }
    for (tenant, dir) in &apps {
        spawn_background_verification(&state, dir, tenant).await;
    }
    spawn_optimization_loop(&state);
    spawn_actor_passivation_loop(&state);
    temper_server::profiling::spawn_continuous_profiler();
    let _pg_actor_runtime_cancel = pg_actor_runtime_cancel;

    // Channel transports: spawn persistent connections to external messaging platforms.
    // Resolve Discord bot token: CLI/env → vault fallback.
    let discord_token_resolved = discord_bot_token.or_else(|| {
        state
            .server
            .secrets_vault
            .as_ref()
            .and_then(|v| v.get_secret(&tenant, "discord_bot_token"))
    });
    if let Some(ref token) = discord_token_resolved {
        // Seed into vault so WASM modules can also access it.
        if let Some(ref vault) = state.server.secrets_vault {
            vault.cache_platform_secret("discord_bot_token", token.clone());
        }
        spawn_channel_transport_discord(
            &state,
            token.clone(),
            &tenant,
            actual_port,
            state.api_token.clone(),
        );
    } else {
        println!("  Discord transport: not configured");
        println!("    Set DISCORD_BOT_TOKEN env var or store 'discord_bot_token' in vault");
    }

    // Where this process reports its earliest due time, so a control plane
    // that suspends it can wake it then. Both are needed; either alone is a
    // misconfiguration, logged and otherwise ignored.
    match (
        std::env::var("TEMPER_WAKE_REPORT_URL").ok(), // determinism-ok: startup config
        std::env::var("TEMPER_WAKE_REPORT_TOKEN").ok(), // determinism-ok: startup config
    ) {
        (Some(url), Some(token)) if !url.is_empty() && !token.is_empty() => {
            match temper_server::wake::HttpWakeSink::new(url, token) {
                Ok(sink) => state.server.set_wake_sink(std::sync::Arc::new(sink)),
                Err(error) => tracing::error!(%error, "wake reports off"),
            }
        }
        (None, None) => {}
        _ => tracing::error!(
            "wake reports off: TEMPER_WAKE_REPORT_URL and TEMPER_WAKE_REPORT_TOKEN are needed together"
        ),
    }
    // How long the sandbox stays awake after an inbound request, in seconds:
    // a timer due before then fires on its own and is not reported. The
    // control plane sets it below the time its sandboxes really stay awake,
    // for margin. Unset, every due time from now is reported.
    if let Ok(awake) = std::env::var("TEMPER_WAKE_AWAKE_SECS") {
        // determinism-ok: startup config
        match awake.parse::<u64>() {
            Ok(secs) => state
                .server
                .set_wake_awake(std::time::Duration::from_secs(secs)),
            Err(error) => {
                tracing::error!(%error, "TEMPER_WAKE_AWAKE_SECS is not a number of seconds; every due time is reported")
            }
        }
    }

    // Phase 10: route tables and the kernel runtime, before the first request.
    boot::start_serving(&state, later).await;

    println!("Listening on http://{bind}:{actual_port}");
    // A stop signal ends the server. The kernel then logs and counts each
    // write it still owes and holds only in memory, and telemetry is flushed
    // so those reports leave the process before it exits.
    tokio::select! {
        served = axum::serve(listener, router).into_future() => served.context("Server error")?,
        () = stop_signal() => {}
    }
    state.server.report_owed_writes_at_shutdown();
    if let Some(guard) = otel_guard {
        guard.shutdown();
    }

    Ok(())
}

/// Resolves when the process is asked to stop: Ctrl-C, or SIGTERM from the
/// platform it runs on. A signal that cannot be watched never resolves.
async fn stop_signal() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                terminate.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

fn cache_platform_secret_if_present(
    vault: &temper_server::secrets::vault::SecretsVault,
    secret_name: &str,
    value: Option<String>,
) {
    if let Some(value) = value {
        vault.cache_platform_secret(secret_name, value);
    }
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

/// Shortest pause before the next passivation tick while idle actors are still
/// waiting. A tick sheds at most `PASSIVATE_IDLE_ACTORS_PER_TICK` actors; an
/// app that creates records faster than that per interval would otherwise
/// grow the registry without end. A backlog is worked off in batches, each
/// followed by four times its own duration, so snapshot writes take at most a
/// fifth of the time (ARN-462). The pause never exceeds the configured
/// interval, so a backlog is never drained slower than ticks already ran.
const PASSIVATION_BACKLOG_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);
const PASSIVATION_BACKLOG_PAUSE_PER_TICK_TIME: u32 = 4;

fn spawn_actor_passivation_loop(state: &PlatformState) {
    let interval_secs = std::env::var("TEMPER_PASSIVATION_CHECK_INTERVAL") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 86_400);
    let interval = std::time::Duration::from_secs(interval_secs);

    let server = state.server.clone();
    tokio::spawn(async move {
        // determinism-ok: background task for resource management
        let mut pause = interval;
        loop {
            tokio::time::sleep(pause).await; // determinism-ok: background resource management
            let started = std::time::Instant::now(); // determinism-ok: paces a background task
            let left = server.passivate_idle_actors().await;
            pause = if left > 0 {
                (started.elapsed() * PASSIVATION_BACKLOG_PAUSE_PER_TICK_TIME)
                    .clamp(PASSIVATION_BACKLOG_PAUSE, interval)
            } else {
                interval
            };
        }
    });
}

/// Spawn the Discord channel transport using the temper-transport crate.
///
/// The transport is an OData API client — it bootstraps Channel + AgentRoute
/// entities on startup, dispatches Channel.ReceiveMessage for inbound messages,
/// and receives replies via a webhook listener that send_reply WASM calls.
fn spawn_channel_transport_discord(
    _state: &PlatformState,
    bot_token: String,
    tenant: &str,
    port: u16,
    api_key: Option<String>,
) {
    use temper_transport::TemperApiConfig;
    use temper_transport::discord::types::intents;
    use temper_transport::discord::{DiscordConfig, DiscordTransport};

    let tenant = tenant.to_string();
    let api_url = format!("http://127.0.0.1:{port}");
    println!("  Discord channel transport (v2): connecting (tenant={tenant})...");
    tokio::spawn(async move {
        // determinism-ok: WebSocket for channel transport
        let api = temper_transport::TemperApiClient::new(TemperApiConfig {
            base_url: api_url,
            tenant,
            api_key,
        });
        let config = DiscordConfig {
            bot_token,
            intents: intents::DEFAULT,
            webhook_port: 0, // Auto-assign
        };
        let transport = DiscordTransport::new(config, api);
        if let Err(e) = transport.run().await {
            eprintln!("  [discord] Transport fatal error: {e}");
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
/// The returned task commits the app's specs once every verification passes.
async fn spawn_background_verification(
    state: &PlatformState,
    specs_dir: &str,
    tenant: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let specs_path = Path::new(specs_dir);
    let mut ioa_sources = match read_ioa_sources(specs_path) {
        Ok(sources) => sources,
        Err(e) => {
            eprintln!("Warning: failed to read IOA sources for background verification: {e}");
            return None;
        }
    };
    // The CSDL the app's rows were stored with, committed with them.
    let csdl_xml = match std::fs::read_to_string(specs_path.join("model.csdl.xml")) {
        Ok(csdl_xml) => csdl_xml,
        Err(e) => {
            eprintln!("Warning: failed to read the CSDL for background verification: {e}");
            return None;
        }
    };

    let registry = state.registry.clone();
    let server = state.server.clone();
    let tenant_str = tenant.to_string();
    let verification_cache = match server.turso_store_for_tenant(tenant).await {
        Some(store) => match store.load_verification_cache(tenant).await {
            Ok(cache) => cache,
            Err(e) => {
                eprintln!(
                    "Warning: failed to load verification cache for background verification ({tenant}): {e}"
                );
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };

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

    // The app's whole spec set, committed once every changed spec verifies.
    let app_sources: BTreeMap<String, String> = ioa_sources
        .iter()
        .map(|(entity, source)| (entity.clone(), source.clone()))
        .collect();
    let source_count = ioa_sources.len();
    ioa_sources = ioa_sources_requiring_background_verification(ioa_sources, &verification_cache);
    let skipped_count = source_count.saturating_sub(ioa_sources.len());
    if skipped_count > 0 {
        println!(
            "  [verify] Skipped {skipped_count} unchanged verified specs for tenant {tenant_str}"
        );
    }

    let mut verifications = Vec::with_capacity(ioa_sources.len());
    for (entity_name, ioa_source) in ioa_sources {
        let registry = registry.clone();
        let server = server.clone();
        let tenant = tenant_str.clone();
        let entity = entity_name.clone();

        verifications.push(tokio::spawn(async move {
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
                    return false;
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
                return false;
            }

            println!("  [verify] Starting verification for {entity}...");

            let entity_clone = entity.clone();

            // Run the cascade: in subprocess (isolated) or in-process (default).
            let result: Result<temper_verify::CascadeResult, String> = if let Some(bin) =
                server.verify_subprocess_bin.as_deref()
            {
                temper_server::observe::subprocess_verify::verify_in_subprocess(bin, &ioa_source)
                    .await
            } else {
                tokio::task::spawn_blocking(move || {
                    VerificationCascade::from_ioa(&ioa_source)
                        .with_sim_seeds(5)
                        .with_prop_test_cases(100)
                        .run()
                })
                .await
                .map_err(|e| e.to_string())
            };

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
                            return false;
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
                    all_passed
                }
                Err(e) => {
                    eprintln!("  [verify] {entity_clone}: verification failed: {e}");

                    let verification_result = EntityVerificationResult {
                        all_passed: false,
                        levels: vec![EntityLevelSummary {
                            level: "Error".to_string(),
                            passed: false,
                            summary: format!("Verification failed: {e}"),
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
                            return false;
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
                            summary: format!("Verification failed: {e}"),
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
                    false
                }
            }
        }));
    }

    // Without a commit, restart recovery keeps the app's last committed specs.
    Some(tokio::spawn(async move {
        for verification in verifications {
            if !verification.await.unwrap_or(false) {
                return;
            }
        }
        match server
            .commit_spec_sources(&tenant_str, &app_sources, &csdl_xml, &[])
            .await
        {
            Ok(Some(true) | None) => {}
            Ok(Some(false)) => eprintln!("Warning: specs for {tenant_str} changed before commit"),
            Err(e) if is_ephemeral_metadata_error(&e) => {}
            Err(e) => eprintln!("Warning: failed to commit verified specs for {tenant_str}: {e}"),
        }
    }))
}

fn ioa_sources_requiring_background_verification(
    ioa_sources: HashMap<String, String>,
    verification_cache: &BTreeMap<String, (String, bool)>,
) -> HashMap<String, String> {
    ioa_sources
        .into_iter()
        .filter(|(entity_name, ioa_source)| {
            !verification_cache
                .get(entity_name)
                .is_some_and(|(cached_hash, verified)| {
                    *verified && cached_hash == &temper_store_turso::spec_content_hash(ioa_source)
                })
        })
        .collect()
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
