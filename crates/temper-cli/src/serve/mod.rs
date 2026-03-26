//! Development server command for `temper serve`.
//!
//! Delegates to `temper-platform` for the hosting platform:
//! OData API for all entities (system + user), evolution engine,
//! and verify-and-deploy pipeline.
//!
//! Specs are loaded immediately (design-time observation) and verification
//! runs in the background so the observe UI can stream progress.

mod bootstrap;
mod loader;
mod storage;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncBufReadExt;

use temper_evolution::PostgresRecordStore;
use temper_observe::ClickHouseStore;
use temper_observe::otel::init_observability;
use temper_platform::optimization::run_optimization_cycle;
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityLevelSummary, EntityVerificationResult, VerificationStatus};
use temper_server::state::DesignTimeEvent;
use temper_verify::cascade::VerificationCascade;

use crate::StorageBackend;

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
    port: u16,
    apps: Vec<(String, String)>,
    skills: Vec<String>,
    storage: StorageBackend,
    storage_explicit: bool,
    observe: bool,
    verify_subprocess: bool,
    discord_bot_token: Option<String>,
    tenant: String,
) -> Result<()> {
    let _otel_guard = init_observability("temper-platform");
    temper_authz::init_metrics();
    temper_store_turso::init_metrics();
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Phase 1: Storage backend
    let (pg_pool, event_store) = bootstrap::init_storage(storage, storage_explicit).await?;

    // Phase 2: Registry (restore + disk apps)
    let (registry, mut tenant_policy_seed) =
        bootstrap::build_registry(pg_pool.as_ref(), &event_store, &apps).await?;

    // Assemble platform state
    let mut state = PlatformState::with_registry(registry, api_key);
    state.api_token = std::env::var("TEMPER_API_KEY").ok();
    if state.api_token.is_some() {
        println!("  API key: configured (Bearer token required)");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let data_dir = Path::new(&home).join(".local/share/temper");
    state.server.data_dir = data_dir.clone();

    // Phase 3: Auto-reload previously registered specs
    let (auto_reloaded, auto_reloaded_policies) = bootstrap::auto_reload_specs(&state, &data_dir);
    tenant_policy_seed.extend(auto_reloaded_policies);

    seed_cedar_policies(&state, tenant_policy_seed);
    println!(
        "  Auto-reloaded {auto_reloaded} specs entries from {}",
        data_dir.join("specs-registry.json").display()
    );
    state.server.rebuild_reaction_dispatcher();

    // Configure subprocess verification if requested.
    if verify_subprocess {
        let bin = std::env::current_exe() // determinism-ok: read once at startup
            .unwrap_or_else(|_| std::path::PathBuf::from("temper"));
        state.server.verify_subprocess_bin = Some(Arc::new(bin));
        println!("  Verification mode: subprocess (30s timeout per entity)");
    }

    // Phase 4: Webhooks
    state.server.webhook_dispatcher = bootstrap::load_webhooks(&apps);

    // Phase 5: Persistence wiring
    if let Some(store) = event_store {
        if let Some(pool) = store.postgres_pool().cloned() {
            state.server.pg_record_store = Some(Arc::new(PostgresRecordStore::new(pool)));
        }
        state.server.event_store = Some(Arc::new(store));
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

    // Phase 6: Entity hydration
    bootstrap::hydrate_entities(&state, &apps).await;

    // Phase 7: Recovery (Cedar policies + WASM modules + secrets)
    bootstrap::recover_cedar_policies(&state).await;
    bootstrap::recover_wasm_modules(&state).await;
    bootstrap::recover_secrets(&state).await;

    // Seed secrets from env into the vault for all tenants.
    if let Some(ref vault) = state.server.secrets_vault {
        // ANTHROPIC_API_KEY — makes {secret:anthropic_api_key} resolve in LLM integrations.
        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            // determinism-ok: env var read at startup for configuration
            let _ = vault.cache_secret("default", "anthropic_api_key", key.clone());
            if tenant != "default" {
                let _ = vault.cache_secret(&tenant, "anthropic_api_key", key);
            }
        }

        // blob_endpoint — points blob_adapter at the server's internal blob storage
        // when no external blob endpoint (R2/S3) is configured.
        // determinism-ok: env var read at startup for configuration
        if std::env::var("BLOB_ENDPOINT").is_err() {
            let blob_url = format!("http://127.0.0.1:{port}/_internal/blobs");
            let _ = vault.cache_secret("default", "blob_endpoint", blob_url.clone());
            if tenant != "default" {
                let _ = vault.cache_secret(&tenant, "blob_endpoint", blob_url);
            }
        }

        // temper_api_url — points WASM modules at this server for TemperFS calls.
        {
            let api_url = format!("http://127.0.0.1:{port}");
            let _ = vault.cache_secret("default", "temper_api_url", api_url.clone());
            if tenant != "default" {
                let _ = vault.cache_secret(&tenant, "temper_api_url", api_url);
            }
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

                // Find the local sandbox script relative to the binary or os-apps.
                let sandbox_script =
                    std::path::Path::new("os-apps/temper-agent/sandbox/local_sandbox.py");
                if sandbox_script.exists() {
                    // Use /tmp/temper-sandbox as the base; create /workspace for tool_runner
                    // which sends cwd="/workspace" by default (matching E2B's layout).
                    let _ = std::fs::create_dir_all("/tmp/temper-sandbox");
                    let _ = std::fs::create_dir_all("/workspace");

                    // determinism-ok: subprocess spawn at startup for local dev sandbox
                    match std::process::Command::new("python3")
                        .arg(sandbox_script)
                        .arg("--port")
                        .arg(sandbox_port.to_string())
                        .arg("--workdir")
                        .arg("/tmp/temper-sandbox")
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                    {
                        Ok(_child) => {
                            println!("  Local sandbox: {sandbox_url} (auto-started)");
                        }
                        Err(e) => {
                            eprintln!("  Warning: failed to start local sandbox: {e}");
                            eprintln!(
                                "  Run manually: python3 {sandbox_script:?} --port {sandbox_port}"
                            );
                        }
                    }
                } else {
                    eprintln!("  Warning: local sandbox script not found at {sandbox_script:?}");
                    eprintln!(
                        "  Set SANDBOX_URL env var or ensure os-apps/temper-agent/sandbox/local_sandbox.py exists"
                    );
                }

                sandbox_url
            };

            let _ = vault.cache_secret("default", "sandbox_url", sandbox_url.clone());
            if tenant != "default" {
                let _ = vault.cache_secret(&tenant, "sandbox_url", sandbox_url);
            }
        }
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

    // Phase 8: Bootstrap system + agent tenants
    bootstrap::bootstrap_tenants(&state, &apps).await;

    // Phase 8b: Restore persisted skills + apply CLI `--skill` requests.
    bootstrap::bootstrap_installed_skills(&state, &skills).await;

    // Phase 9: Bind, start background tasks, serve
    let router = build_platform_router(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .with_context(|| format!("Failed to bind to port {port}"))?;
    let actual_port = listener
        .local_addr()
        .context("Failed to get listener local address")?
        .port();
    let _ = state.server.listen_port.set(actual_port);

    if observe {
        spawn_observe_ui(actual_port);
    }
    for (tenant, dir) in &apps {
        spawn_background_verification(&state, dir, tenant).await;
    }
    spawn_optimization_loop(&state);
    spawn_actor_passivation_loop(&state);
    state.server.spawn_runtime_metrics_loop();

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
            let _ = vault.cache_secret("default", "discord_bot_token", token.clone());
            if tenant != "default" {
                let _ = vault.cache_secret(&tenant, "discord_bot_token", token.clone());
            }
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

    println!("Listening on http://0.0.0.0:{actual_port}");
    axum::serve(listener, router)
        .await
        .context("Server error")?;

    Ok(())
}

fn seed_cedar_policies(state: &PlatformState, tenant_policy_seed: BTreeMap<String, String>) {
    for (tenant, policy_text) in &tenant_policy_seed {
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(tenant, policy_text)
        {
            eprintln!("  Warning: failed to load Cedar policies for tenant '{tenant}': {e}");
            continue;
        }
    }
    // Update in-memory text cache.
    let mut policies = state.server.tenant_policies.write().unwrap(); // ci-ok: infallible lock
    for (tenant, policy_text) in tenant_policy_seed {
        policies.insert(tenant, policy_text);
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

fn spawn_actor_passivation_loop(state: &PlatformState) {
    let interval_secs = std::env::var("TEMPER_PASSIVATION_CHECK_INTERVAL") // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 86_400);

    let server = state.server.clone();
    tokio::spawn(async move {
        // determinism-ok: background task for resource management
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume immediate tick

        loop {
            ticker.tick().await;
            server.passivate_idle_actors().await;
        }
    });
}

/// Spawn the Observe UI (Next.js dev server) in the background.
///
/// Looks for the `ui/observe/` directory relative to the binary or cwd.
/// Falls back gracefully if npm/node_modules are unavailable.
fn spawn_observe_ui(api_port: u16) {
    // The workspace root is embedded at compile time so `temper serve` works
    // regardless of the current working directory.
    const WORKSPACE_ROOT: &str = env!("CARGO_MANIFEST_DIR");

    // Try to find the observe directory from multiple locations.
    let observe_dir = None
        .or_else(|| {
            // Compile-time path: workspace_root/../../ui/observe (CARGO_MANIFEST_DIR
            // points to crates/temper-cli, so go up twice).
            let d = Path::new(WORKSPACE_ROOT)
                .parent()?
                .parent()?
                .join("ui/observe");
            if d.exists() { Some(d) } else { None }
        })
        .or_else(|| {
            // Running from the project root.
            let d = std::env::current_dir().ok()?.join("ui/observe");
            if d.exists() { Some(d) } else { None }
        })
        .or_else(|| {
            // Walk up from cwd to find a repo root containing ui/observe.
            let mut dir = std::env::current_dir().ok()?;
            loop {
                let candidate = dir.join("ui/observe");
                if candidate.exists() {
                    return Some(candidate);
                }
                if dir.join(".git").exists() {
                    return None;
                }
                if !dir.pop() {
                    return None;
                }
            }
        });

    let Some(observe_dir) = observe_dir else {
        eprintln!("  Warning: ui/observe/ directory not found, skipping Observe UI");
        return;
    };

    if !observe_dir.join("node_modules").exists() {
        eprintln!(
            "  Warning: ui/observe/node_modules not found. Run `npm install` in {} first.",
            observe_dir.display()
        );
        return;
    }

    // Find an available port starting from api_port + 1.
    // Next.js respects the PORT env var.
    let observe_port = {
        let mut port = api_port.saturating_add(1);
        loop {
            match std::net::TcpListener::bind(("0.0.0.0", port)) {
                Ok(_listener) => break port, // port is free; listener drops and releases it
                Err(_) => {
                    port = port.saturating_add(1);
                    if port > api_port.saturating_add(20) {
                        eprintln!(
                            "  Warning: no free port found for Observe UI (tried {}-{}), skipping",
                            api_port.saturating_add(1),
                            port
                        );
                        return;
                    }
                }
            }
        }
    };
    println!("  Observe UI: http://localhost:{observe_port}");
    println!();

    tokio::spawn(async move {
        let result = tokio::process::Command::new("npm")
            .arg("run")
            .arg("dev")
            .env("TEMPER_API_URL", format!("http://127.0.0.1:{api_port}"))
            .env("NEXT_PUBLIC_TEMPER_API_PORT", api_port.to_string())
            .env("PORT", observe_port.to_string())
            .current_dir(&observe_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        match result {
            Ok(mut child) => {
                use std::sync::Arc;
                use std::sync::atomic::{AtomicBool, Ordering};

                let opened = Arc::new(AtomicBool::new(false));

                // Drain stdout — watch for the Next.js "Ready" signal.
                if let Some(stdout) = child.stdout.take() {
                    let opened = opened.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if !opened.load(Ordering::Relaxed)
                                && (line.contains("Ready in") || line.contains("started server on"))
                            {
                                opened.store(true, Ordering::Relaxed);
                                let _ = open::that(format!("http://localhost:{observe_port}"));
                            }
                        }
                    });
                }

                // Drain stderr — report errors.
                if let Some(stderr) = child.stderr.take() {
                    let opened = opened.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            // Next.js may also signal readiness on stderr.
                            if !opened.load(Ordering::Relaxed)
                                && (line.contains("Ready in") || line.contains("started server on"))
                            {
                                opened.store(true, Ordering::Relaxed);
                                let _ = open::that(format!("http://localhost:{observe_port}"));
                            }
                            if line.contains("error") || line.contains("Error") {
                                eprintln!("  [observe] {line}");
                            }
                        }
                    });
                }

                let _ = child.wait().await;
            }
            Err(e) => {
                eprintln!("  Warning: failed to start Observe UI: {e}");
            }
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
                }
            }
        });
    }
}
