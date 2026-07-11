use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use temper_evolution::PostgresRecordStore;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{
    FeatureRequestRow, TursoEventStore, TursoInstalledAppRow, TursoSpecRow, spec_content_hash,
};

const GLOBAL_TENANT: &str = "*";

/// Bound on rows fetched from any single Turso source table.
///
/// The Turso list APIs used here (`load_recent_trajectories`,
/// `load_recent_wasm_invocations`, `list_design_time_events`,
/// `list_ots_trajectories`, `list_blobs`) are LIMIT-only — they expose no
/// offset/keyset pagination — so every fetched row is held in memory at once.
/// This bound keeps the tool from OOMing on large tenants. Hitting the bound
/// aborts the migration loudly (see [`ensure_row_bound`]) instead of silently
/// truncating, which would break the manifest checksum guarantees.
const MAX_ROWS: i64 = 100_000;

/// Fail loudly when a table load fills the entire [`MAX_ROWS`] budget: the
/// result may be truncated, and migrating it would silently drop source rows.
fn ensure_row_bound(table: &str, fetched: usize) -> Result<()> {
    if fetched as i64 >= MAX_ROWS {
        return Err(anyhow!(
            "source table {table} returned {fetched} rows, filling the migration row bound of \
             {MAX_ROWS}; aborting instead of migrating a possibly-truncated result. The Turso \
             list APIs do not paginate, so raise MAX_ROWS (and provision memory accordingly) or \
             migrate this table out-of-band"
        ));
    }
    Ok(())
}

pub(crate) struct MigrationOptions {
    pub tenant: String,
    pub dry_run: bool,
    pub verify: bool,
    pub from_snapshot: bool,
    pub manifest_path: PathBuf,
    pub turso_url: Option<String>,
    pub turso_auth_token: Option<String>,
    pub database_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct MigrationManifest {
    started_at: String,
    finished_at: Option<String>,
    source: String,
    target: String,
    requested_tenant: String,
    tenants: Vec<String>,
    dry_run: bool,
    verify: bool,
    from_snapshot: bool,
    tables: Vec<TableManifest>,
    warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct TableManifest {
    tenant: String,
    table: String,
    source_rows: u64,
    target_rows: Option<u64>,
    source_checksum: String,
    target_checksum: Option<String>,
    verified: Option<bool>,
}

#[derive(Default)]
struct ManifestBuilder {
    tables: BTreeMap<(String, String), TableManifest>,
    warnings: Vec<String>,
}

impl ManifestBuilder {
    fn record_source(&mut self, tenant: &str, table: &str, values: Vec<Value>) -> Result<()> {
        let source_rows = values.len() as u64;
        let source_checksum = checksum_values(values)?;
        let key = (tenant.to_string(), table.to_string());
        self.tables.insert(
            key,
            TableManifest {
                tenant: tenant.to_string(),
                table: table.to_string(),
                source_rows,
                target_rows: None,
                source_checksum,
                target_checksum: None,
                verified: None,
            },
        );
        Ok(())
    }

    fn record_target(
        &mut self,
        tenant: &str,
        table: &str,
        values: Vec<Value>,
        enforce: bool,
    ) -> Result<()> {
        let target_rows = values.len() as u64;
        let target_checksum = checksum_values(values)?;
        let Some(entry) = self
            .tables
            .get_mut(&(tenant.to_string(), table.to_string()))
        else {
            return Ok(());
        };
        let verified = entry.source_rows == target_rows && entry.source_checksum == target_checksum;
        entry.target_rows = Some(target_rows);
        entry.target_checksum = Some(target_checksum);
        entry.verified = Some(verified);
        if enforce && !verified {
            return Err(anyhow!(
                "migration verification failed for tenant {tenant} table {table}: source rows/checksum {} {} != target rows/checksum {} {}",
                entry.source_rows,
                entry.source_checksum,
                target_rows,
                entry.target_checksum.as_deref().unwrap_or_default(),
            ));
        }
        Ok(())
    }

    fn warn(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }
}

pub(crate) async fn run(options: MigrationOptions) -> Result<()> {
    let turso_url = required_value(options.turso_url.clone(), "TURSO_URL", "--turso-url")?;
    let database_url = required_value(
        options.database_url.clone(),
        "DATABASE_URL",
        "--database-url",
    )?;
    let turso_auth_token = options
        .turso_auth_token
        .clone()
        .or_else(|| std::env::var("TURSO_AUTH_TOKEN").ok());

    eprintln!("Connecting to Turso source: {}", redact_url(&turso_url));
    let source = TursoEventStore::new(&turso_url, turso_auth_token.as_deref())
        .await
        .context("failed to connect to Turso source")?;

    eprintln!(
        "Connecting to Postgres target: {}",
        redact_url(&database_url)
    );
    let pool = PgPool::connect(&database_url)
        .await
        .context("failed to connect to Postgres target")?;
    temper_store_postgres::migration::run_migrations(&pool)
        .await
        .context("failed to run Postgres store migrations")?;
    PostgresRecordStore::new(pool.clone())
        .migrate()
        .await
        .context("failed to migrate evolution record store")?;
    let target = PostgresEventStore::new(pool.clone());

    let tenants = discover_tenants(&source, &options.tenant).await?;
    eprintln!(
        "Migrating {} tenant(s): {}",
        tenants.len(),
        tenants.join(", ")
    );
    if options.dry_run {
        eprintln!("Dry-run mode: source data will be read and verified, but not written.");
    }

    let started_at = Utc::now();
    let mut builder = ManifestBuilder::default();

    for tenant in &tenants {
        migrate_event_journal(
            &source,
            &target,
            tenant,
            options.from_snapshot,
            options.dry_run,
            &mut builder,
        )
        .await
        .with_context(|| format!("failed migrating event journal for tenant {tenant}"))?;

        migrate_tenant_platform_tables(
            &source,
            &target,
            &pool,
            tenant,
            options.dry_run,
            &mut builder,
        )
        .await
        .with_context(|| format!("failed migrating platform tables for tenant {tenant}"))?;
    }

    migrate_global_platform_tables(&source, &pool, options.dry_run, &mut builder)
        .await
        .context("failed migrating global platform tables")?;

    if options.verify || options.dry_run {
        if options.dry_run {
            builder.warn(
                "dry-run requested; target checksums describe the current target state and are not enforced",
            );
        }
        verify_manifest_tables(&pool, options.verify && !options.dry_run, &mut builder).await?;
    }

    let mut manifest = MigrationManifest {
        started_at: started_at.to_rfc3339(),
        finished_at: Some(Utc::now().to_rfc3339()),
        source: redact_url(&turso_url),
        target: redact_url(&database_url),
        requested_tenant: options.tenant.clone(),
        tenants,
        dry_run: options.dry_run,
        verify: options.verify,
        from_snapshot: options.from_snapshot,
        tables: builder.tables.into_values().collect(),
        warnings: builder.warnings,
    };
    manifest
        .tables
        .sort_by(|a, b| (&a.tenant, &a.table).cmp(&(&b.tenant, &b.table)));

    let json = serde_json::to_string_pretty(&manifest)?;
    tokio::fs::write(&options.manifest_path, json)
        .await
        .with_context(|| {
            format!(
                "failed to write migration manifest to {}",
                options.manifest_path.display()
            )
        })?;
    eprintln!(
        "Migration manifest written to {}",
        options.manifest_path.display()
    );

    Ok(())
}

async fn discover_tenants(source: &TursoEventStore, requested: &str) -> Result<Vec<String>> {
    if requested != "all" {
        return Ok(vec![requested.to_string()]);
    }

    let tenants = source
        .list_storage_tenants()
        .await
        .context("failed to discover source tenants")?;
    if tenants.is_empty() {
        Ok(vec!["default".to_string()])
    } else {
        Ok(tenants)
    }
}

async fn migrate_event_journal(
    source: &TursoEventStore,
    target: &PostgresEventStore,
    tenant: &str,
    from_snapshot: bool,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let mut event_values = Vec::new();
    let mut snapshot_values = Vec::new();
    let entities = source
        .list_entity_ids(tenant)
        .await
        .with_context(|| format!("failed to list source entities for tenant {tenant}"))?;

    for (entity_type, entity_id) in entities {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let events = source
            .read_events(&persistence_id, 0)
            .await
            .with_context(|| format!("failed to read source events for {persistence_id}"))?;
        event_values.extend(
            events
                .iter()
                .map(|event| event_value(tenant, &entity_type, &entity_id, event))
                .collect::<Result<Vec<_>>>()?,
        );
        if !dry_run {
            append_missing_events(target, &persistence_id, &events).await?;
        }

        if from_snapshot
            && let Some((sequence_nr, snapshot)) = source
                .load_snapshot(&persistence_id)
                .await
                .with_context(|| format!("failed to read source snapshot for {persistence_id}"))?
        {
            snapshot_values.push(json!({
                "tenant": tenant,
                "entity_type": entity_type,
                "entity_id": entity_id,
                "sequence_nr": sequence_nr,
                "state": base64::engine::general_purpose::STANDARD.encode(&snapshot),
            }));
            if !dry_run {
                target
                    .save_snapshot(&persistence_id, sequence_nr, &snapshot)
                    .await
                    .with_context(|| {
                        format!("failed to write target snapshot for {persistence_id}")
                    })?;
            }
        }
    }

    builder.record_source(tenant, "events", event_values)?;
    if from_snapshot {
        builder.record_source(tenant, "snapshots", snapshot_values)?;
    }
    Ok(())
}

async fn append_missing_events(
    target: &PostgresEventStore,
    persistence_id: &str,
    source_events: &[PersistenceEnvelope],
) -> Result<()> {
    let target_events = target
        .read_events(persistence_id, 0)
        .await
        .with_context(|| format!("failed to read target events for {persistence_id}"))?;
    let target_len = target_events.len();
    if target_len > source_events.len() {
        return Err(anyhow!(
            "target has more events than source for {persistence_id}: target={target_len}, source={}",
            source_events.len()
        ));
    }
    if target_len == source_events.len() {
        return Ok(());
    }
    target
        .append(
            persistence_id,
            target_len as u64,
            &source_events[target_len..],
        )
        .await
        .with_context(|| format!("failed to append target events for {persistence_id}"))?;
    Ok(())
}

async fn migrate_tenant_platform_tables(
    source: &TursoEventStore,
    target: &PostgresEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    migrate_specs(source, pool, tenant, dry_run, builder).await?;
    migrate_tenant_policies(source, pool, tenant, dry_run, builder).await?;
    migrate_policies(source, pool, tenant, dry_run, builder).await?;
    migrate_tenant_constraints(source, pool, tenant, dry_run, builder).await?;
    migrate_installed_apps(source, pool, tenant, dry_run, builder).await?;
    migrate_trajectories(source, pool, tenant, dry_run, builder).await?;
    migrate_pending_decisions(source, pool, tenant, dry_run, builder).await?;
    migrate_wasm_modules(source, pool, tenant, dry_run, builder).await?;
    migrate_wasm_invocations(source, pool, tenant, dry_run, builder).await?;
    migrate_design_time_events(source, pool, tenant, dry_run, builder).await?;
    migrate_ots_trajectories(source, pool, tenant, dry_run, builder).await?;
    migrate_secrets(source, pool, tenant, dry_run, builder).await?;
    migrate_policy_denial_patterns(source, pool, tenant, dry_run, builder).await?;
    migrate_query_projections(source, target, tenant, dry_run, builder).await?;
    migrate_feature_requests(source, pool, tenant, dry_run, builder).await?;
    migrate_evolution_records(source, pool, tenant, dry_run, builder).await?;
    Ok(())
}

async fn migrate_global_platform_tables(
    source: &TursoEventStore,
    pool: &PgPool,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    migrate_blobs(source, pool, dry_run, builder).await?;
    Ok(())
}

async fn migrate_specs(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.load_specs().await?;
    let rows: Vec<TursoSpecRow> = rows
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect();
    let values = rows.iter().map(spec_value).collect::<Result<Vec<_>>>()?;
    builder.record_source(tenant, "specs", values)?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let verification_result = row
            .verification_result
            .as_deref()
            .map(json_or_string)
            .transpose()?;
        let content_hash = row
            .content_hash
            .clone()
            .filter(|hash| !hash.is_empty())
            .unwrap_or_else(|| spec_content_hash(&row.ioa_source));
        sqlx::query(
            "INSERT INTO specs \
             (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, \
              levels_passed, levels_total, verification_result, content_hash, committed, updated_at) \
             VALUES ($1, $2, $3, $4, 1, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (tenant, entity_type) DO UPDATE SET \
                 ioa_source = EXCLUDED.ioa_source, csdl_xml = EXCLUDED.csdl_xml, verified = EXCLUDED.verified, \
                 verification_status = EXCLUDED.verification_status, levels_passed = EXCLUDED.levels_passed, \
                 levels_total = EXCLUDED.levels_total, verification_result = EXCLUDED.verification_result, \
                 content_hash = EXCLUDED.content_hash, committed = EXCLUDED.committed, updated_at = EXCLUDED.updated_at",
        )
        .bind(&row.tenant)
        .bind(&row.entity_type)
        .bind(&row.ioa_source)
        .bind(&row.csdl_xml)
        .bind(row.verified)
        .bind(&row.verification_status)
        .bind(row.levels_passed)
        .bind(row.levels_total)
        .bind(verification_result)
        .bind(content_hash)
        .bind(row.committed)
        .bind(parse_source_timestamp(&row.updated_at)?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_tenant_policies(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows: Vec<(String, String)> = source
        .load_tenant_policies()
        .await?
        .into_iter()
        .filter(|(row_tenant, _)| row_tenant == tenant)
        .collect();
    builder.record_source(
        tenant,
        "tenant_policies",
        rows.iter()
            .map(|(tenant, policy_text)| json!({ "tenant": tenant, "policy_text": policy_text }))
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for (tenant, policy_text) in rows {
        sqlx::query(
            "INSERT INTO tenant_policies (tenant, policy_text, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (tenant) DO UPDATE SET policy_text = EXCLUDED.policy_text, updated_at = now()",
        )
        .bind(tenant)
        .bind(policy_text)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_policies(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .load_all_policies()
        .await?
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    builder.record_source(
        tenant,
        "policies",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "policy_id": row.policy_id,
                    "cedar_text": row.cedar_text,
                    "policy_hash": row.policy_hash,
                    "created_by": row.created_by,
                    "enabled": row.enabled,
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        sqlx::query(
            "INSERT INTO policies (tenant, policy_id, cedar_text, policy_hash, created_at, created_by, enabled) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (tenant, policy_id) DO UPDATE SET \
                 cedar_text = EXCLUDED.cedar_text, policy_hash = EXCLUDED.policy_hash, \
                 created_at = EXCLUDED.created_at, created_by = EXCLUDED.created_by, enabled = EXCLUDED.enabled",
        )
        .bind(row.tenant)
        .bind(row.policy_id)
        .bind(row.cedar_text)
        .bind(row.policy_hash)
        .bind(parse_source_timestamp(&row.created_at)?)
        .bind(row.created_by)
        .bind(row.enabled)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_tenant_constraints(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .load_tenant_constraints()
        .await?
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    builder.record_source(
        tenant,
        "tenant_constraints",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "cross_invariants_toml": row.cross_invariants_toml,
                    "version": row.version,
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        sqlx::query(
            "INSERT INTO tenant_constraints (tenant, cross_invariants_toml, version, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant) DO UPDATE SET \
                 cross_invariants_toml = EXCLUDED.cross_invariants_toml, version = EXCLUDED.version, updated_at = EXCLUDED.updated_at",
        )
        .bind(row.tenant)
        .bind(row.cross_invariants_toml)
        .bind(row.version)
        .bind(parse_source_timestamp(&row.updated_at)?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_installed_apps(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let mut rows = Vec::new();
    for (row_tenant, app_name) in source.list_all_installed_apps().await? {
        if row_tenant == tenant
            && let Some(row) = source.get_installed_app(&row_tenant, &app_name).await?
        {
            rows.push(row);
        }
    }
    builder.record_source(
        tenant,
        "tenant_installed_apps",
        rows.iter().map(installed_app_value).collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        insert_installed_app(pool, &row).await?;
    }
    Ok(())
}

async fn insert_installed_app(pool: &PgPool, row: &TursoInstalledAppRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO tenant_installed_apps \
         (tenant, app_name, source_kind, app_ref, version_hash, pinned_version_hash, current_version_hash, follow_policy, \
          closure_id, registry_url, registry_tenant, app_version, bundle_digest, spec_digest, policy_digest, wasm_digest, \
          content_digest, seed_digest, installed_at, last_reconciled_at, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21) \
         ON CONFLICT (tenant, app_name) DO UPDATE SET \
             source_kind = EXCLUDED.source_kind, app_ref = EXCLUDED.app_ref, version_hash = EXCLUDED.version_hash, \
             pinned_version_hash = EXCLUDED.pinned_version_hash, current_version_hash = EXCLUDED.current_version_hash, \
             follow_policy = EXCLUDED.follow_policy, closure_id = EXCLUDED.closure_id, \
             registry_url = EXCLUDED.registry_url, registry_tenant = EXCLUDED.registry_tenant, \
             app_version = EXCLUDED.app_version, bundle_digest = EXCLUDED.bundle_digest, \
             spec_digest = EXCLUDED.spec_digest, policy_digest = EXCLUDED.policy_digest, \
             wasm_digest = EXCLUDED.wasm_digest, content_digest = EXCLUDED.content_digest, \
             seed_digest = EXCLUDED.seed_digest, installed_at = EXCLUDED.installed_at, \
             last_reconciled_at = EXCLUDED.last_reconciled_at, status = EXCLUDED.status",
    )
    .bind(&row.tenant_id)
    .bind(&row.app_name)
    .bind(&row.source_kind)
    .bind(&row.app_ref)
    .bind(&row.version_hash)
    .bind(&row.pinned_version_hash)
    .bind(&row.current_version_hash)
    .bind(&row.follow_policy)
    .bind(&row.closure_id)
    .bind(&row.registry_url)
    .bind(&row.registry_tenant)
    .bind(&row.app_version)
    .bind(&row.bundle_digest)
    .bind(&row.spec_digest)
    .bind(&row.policy_digest)
    .bind(&row.wasm_digest)
    .bind(&row.content_digest)
    .bind(&row.seed_digest)
    .bind(parse_source_timestamp_or_now(&row.installed_at))
    .bind(parse_optional_source_timestamp(
        row.last_reconciled_at.as_deref(),
    )?)
    .bind(&row.status)
    .execute(pool)
    .await?;
    Ok(())
}

async fn migrate_trajectories(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.load_recent_trajectories(tenant, MAX_ROWS).await?;
    ensure_row_bound("trajectories", rows.len())?;
    builder.record_source(
        tenant,
        "trajectories",
        rows.iter()
            .map(trajectory_value)
            .collect::<Result<Vec<_>>>()?,
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let request_body = row
            .request_body
            .as_deref()
            .map(json_or_string)
            .transpose()?;
        let matched_policy_ids = row
            .matched_policy_ids
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        sqlx::query(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, agent_id, session_id, \
              authz_denied, denied_resource, denied_module, source, spec_governed, created_at, request_body, intent, matched_policy_ids) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19 \
             WHERE NOT EXISTS ( \
               SELECT 1 FROM trajectories \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND action = $4 \
                 AND created_at = $16 AND COALESCE(agent_id, '') = COALESCE($9, '') \
                 AND COALESCE(session_id, '') = COALESCE($10, '') AND COALESCE(error, '') = COALESCE($8, '') \
             )",
        )
        .bind(row.tenant)
        .bind(row.entity_type)
        .bind(row.entity_id)
        .bind(row.action)
        .bind(row.success)
        .bind(row.from_status)
        .bind(row.to_status)
        .bind(row.error)
        .bind(row.agent_id)
        .bind(row.session_id)
        .bind(row.authz_denied)
        .bind(row.denied_resource)
        .bind(row.denied_module)
        .bind(row.source)
        .bind(row.spec_governed)
        .bind(parse_source_timestamp(&row.created_at)?)
        .bind(request_body)
        .bind(row.intent)
        .bind(matched_policy_ids)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_pending_decisions(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .query_all_decisions(None)
        .await?
        .into_iter()
        .map(|data| decision_value(&data))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|value| value.get("tenant").and_then(Value::as_str) == Some(tenant))
        .collect::<Vec<_>>();
    builder.record_source(tenant, "pending_decisions", rows.clone())?;
    if dry_run {
        return Ok(());
    }
    for value in rows {
        sqlx::query(
            "INSERT INTO pending_decisions (id, tenant, status, data, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (id) DO UPDATE SET tenant = EXCLUDED.tenant, status = EXCLUDED.status, data = EXCLUDED.data, updated_at = now()",
        )
        .bind(value.get("id").and_then(Value::as_str).unwrap_or_default())
        .bind(value.get("tenant").and_then(Value::as_str).unwrap_or("default"))
        .bind(value.get("status").and_then(Value::as_str).unwrap_or("pending"))
        .bind(value.get("data").cloned().unwrap_or(Value::Null))
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_wasm_modules(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .load_wasm_modules_all_tenants()
        .await?
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    builder.record_source(
        tenant,
        "wasm_modules",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "module_name": row.module_name,
                    "sha256_hash": row.sha256_hash,
                    "size_bytes": row.size_bytes,
                    "wasm_bytes": base64::engine::general_purpose::STANDARD.encode(&row.wasm_bytes),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        sqlx::query(
            "INSERT INTO wasm_modules (tenant, module_name, wasm_bytes, sha256_hash, version, size_bytes, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (tenant, module_name) DO UPDATE SET wasm_bytes = EXCLUDED.wasm_bytes, \
                 sha256_hash = EXCLUDED.sha256_hash, version = EXCLUDED.version, size_bytes = EXCLUDED.size_bytes, updated_at = EXCLUDED.updated_at",
        )
        .bind(row.tenant)
        .bind(row.module_name)
        .bind(row.wasm_bytes)
        .bind(row.sha256_hash)
        .bind(row.version)
        .bind(row.size_bytes)
        .bind(parse_source_timestamp(&row.updated_at)?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_wasm_invocations(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let all_rows = source.load_recent_wasm_invocations(MAX_ROWS).await?;
    ensure_row_bound("wasm_invocation_logs", all_rows.len())?;
    let rows = all_rows
        .into_iter()
        .filter(|row| row.tenant == tenant)
        .collect::<Vec<_>>();
    builder.record_source(
        tenant,
        "wasm_invocation_logs",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "entity_type": row.entity_type,
                    "entity_id": row.entity_id,
                    "module_name": row.module_name,
                    "trigger_action": row.trigger_action,
                    "callback_action": row.callback_action,
                    "success": row.success,
                    "error": row.error,
                    "duration_ms": row.duration_ms,
                    "created_at": normalize_timestamp_for_value(&row.created_at),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let created_at = parse_source_timestamp(&row.created_at)?;
        sqlx::query(
            "INSERT INTO wasm_invocation_logs \
             (tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10 \
             WHERE NOT EXISTS ( \
               SELECT 1 FROM wasm_invocation_logs \
               WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND module_name = $4 \
                 AND trigger_action = $5 AND created_at = $10 \
             )",
        )
        .bind(row.tenant)
        .bind(row.entity_type)
        .bind(row.entity_id)
        .bind(row.module_name)
        .bind(row.trigger_action)
        .bind(row.callback_action)
        .bind(row.success)
        .bind(row.error)
        .bind(row.duration_ms as i64)
        .bind(created_at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_design_time_events(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .list_design_time_events(Some(tenant), MAX_ROWS)
        .await?;
    ensure_row_bound("design_time_events", rows.len())?;
    builder.record_source(
        tenant,
        "design_time_events",
        rows.iter()
            .map(|row| {
                json!({
                    "kind": row.kind,
                    "entity_type": row.entity_type,
                    "tenant": row.tenant,
                    "summary": row.summary,
                    "level": row.level,
                    "passed": row.passed,
                    "step_number": row.step_number,
                    "total_steps": row.total_steps,
                    "created_at": normalize_timestamp_for_value(&row.created_at),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let created_at = parse_source_timestamp(&row.created_at)?;
        sqlx::query(
            "INSERT INTO design_time_events \
             (kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9 \
             WHERE NOT EXISTS ( \
               SELECT 1 FROM design_time_events \
               WHERE kind = $1 AND entity_type = $2 AND tenant = $3 AND summary = $4 AND created_at = $9 \
             )",
        )
        .bind(row.kind)
        .bind(row.entity_type)
        .bind(row.tenant)
        .bind(row.summary)
        .bind(row.level)
        .bind(row.passed)
        .bind(row.step_number.map(|v| v as i16))
        .bind(row.total_steps.map(|v| v as i16))
        .bind(created_at)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_ots_trajectories(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .list_ots_trajectories(tenant, None, None, MAX_ROWS)
        .await?;
    ensure_row_bound("ots_trajectories", rows.len())?;
    let mut values = Vec::new();
    let mut rows_with_data = Vec::new();
    for row in rows {
        let data = source
            .get_ots_trajectory(&row.trajectory_id)
            .await?
            .unwrap_or_else(|| "{}".to_string());
        values.push(json!({
            "trajectory_id": row.trajectory_id,
            "tenant": row.tenant,
            "agent_id": row.agent_id,
            "session_id": row.session_id,
            "outcome": row.outcome,
            "turn_count": row.turn_count,
            "data": json_or_string(&data)?,
            "created_at": normalize_timestamp_for_value(&row.created_at),
        }));
        rows_with_data.push((row, data));
    }
    builder.record_source(tenant, "ots_trajectories", values)?;
    if dry_run {
        return Ok(());
    }
    for (row, data) in rows_with_data {
        sqlx::query(
            "INSERT INTO ots_trajectories \
             (trajectory_id, tenant, agent_id, session_id, outcome, turn_count, data, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (trajectory_id) DO UPDATE SET tenant = EXCLUDED.tenant, agent_id = EXCLUDED.agent_id, \
                 session_id = EXCLUDED.session_id, outcome = EXCLUDED.outcome, turn_count = EXCLUDED.turn_count, \
                 data = EXCLUDED.data, created_at = EXCLUDED.created_at",
        )
        .bind(row.trajectory_id)
        .bind(row.tenant)
        .bind(row.agent_id)
        .bind(row.session_id)
        .bind(row.outcome)
        .bind(row.turn_count)
        .bind(json_or_string(&data)?)
        .bind(parse_source_timestamp(&row.created_at)?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_secrets(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.load_secrets_for_tenant(tenant).await?;
    builder.record_source(
        tenant,
        "tenant_secrets",
        rows.iter()
            .map(|(key_name, ciphertext, nonce)| {
                json!({
                    "tenant": tenant,
                    "key_name": key_name,
                    "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext),
                    "nonce": base64::engine::general_purpose::STANDARD.encode(nonce),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for (key_name, ciphertext, nonce) in rows {
        sqlx::query(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, now(), now()) \
             ON CONFLICT (tenant, key_name) DO UPDATE SET ciphertext = EXCLUDED.ciphertext, nonce = EXCLUDED.nonce, updated_at = now()",
        )
        .bind(tenant)
        .bind(key_name)
        .bind(ciphertext)
        .bind(nonce)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn migrate_policy_denial_patterns(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.load_policy_denial_patterns(tenant).await?;
    builder.record_source(
        tenant,
        "policy_denial_patterns",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "agent_type": row.agent_type,
                    "action": row.action,
                    "resource_type": row.resource_type,
                    "count": row.count,
                    "first_seen": normalize_timestamp_for_value(&row.first_seen),
                    "last_seen": normalize_timestamp_for_value(&row.last_seen),
                    "distinct_resource_ids_json": json_or_string(&row.distinct_resource_ids_json).unwrap_or(Value::Array(vec![])),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        sqlx::query(
            "INSERT INTO policy_denial_patterns \
             (tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (tenant, agent_type, action, resource_type) DO UPDATE SET \
                 count = EXCLUDED.count, first_seen = EXCLUDED.first_seen, last_seen = EXCLUDED.last_seen, \
                 distinct_resource_ids_json = EXCLUDED.distinct_resource_ids_json",
        )
        .bind(row.tenant)
        .bind(row.agent_type.unwrap_or_default())
        .bind(row.action)
        .bind(row.resource_type)
        .bind(row.count)
        .bind(parse_source_timestamp(&row.first_seen)?)
        .bind(parse_source_timestamp(&row.last_seen)?)
        .bind(json_or_string(&row.distinct_resource_ids_json)?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Migrate `entity_catalog` rows via `upsert_query_projection`.
///
/// `entity_field_index` is intentionally NOT migrated as a table: it is
/// derived data. The Postgres `upsert_query_projection` rebuilds the per-field
/// index rows from the projected `fields` JSON inside the same transaction
/// (see `reconcile_query_projection_field_index` in
/// `temper-store-postgres/src/platform/projection.rs`), so migrating the
/// catalog repopulates the field index as a side effect.
async fn migrate_query_projections(
    source: &TursoEventStore,
    target: &PostgresEventStore,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.export_query_projections(Some(tenant)).await?;
    builder.record_source(
        tenant,
        "entity_catalog",
        rows.iter()
            .map(|row| {
                json!({
                    "tenant": row.tenant,
                    "entity_type": row.entity_type,
                    "entity_id": row.entity_id,
                    "status": row.status,
                    "fields": row.fields,
                    "sequence_nr": row.sequence_nr,
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        target
            .upsert_query_projection(
                &row.tenant,
                &row.entity_type,
                &row.entity_id,
                &row.status,
                &row.fields,
                row.sequence_nr,
            )
            .await?;
    }
    Ok(())
}

async fn migrate_feature_requests(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.list_feature_requests(tenant, None).await?;
    builder.record_source(
        tenant,
        "feature_requests",
        rows.iter()
            .map(feature_request_value)
            .collect::<Result<Vec<_>>>()?,
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let result = sqlx::query(
            "INSERT INTO feature_requests \
             (id, tenant, category, description, frequency, trajectory_refs, disposition, developer_notes, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (id) DO UPDATE SET category = EXCLUDED.category, description = EXCLUDED.description, \
                 frequency = EXCLUDED.frequency, trajectory_refs = EXCLUDED.trajectory_refs, disposition = EXCLUDED.disposition, \
                 developer_notes = EXCLUDED.developer_notes, created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at \
             WHERE feature_requests.tenant = EXCLUDED.tenant",
        )
        .bind(row.id)
        .bind(row.tenant)
        .bind(row.category)
        .bind(row.description)
        .bind(row.frequency)
        .bind(json_or_string(&row.trajectory_refs)?)
        .bind(row.disposition)
        .bind(row.developer_notes)
        .bind(parse_source_timestamp(&row.created_at)?)
        .bind(parse_source_timestamp(&row.updated_at)?)
        .execute(pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(anyhow!(
                "feature request ID collision across tenants during migration"
            ));
        }
    }
    Ok(())
}

async fn migrate_evolution_records(
    source: &TursoEventStore,
    pool: &PgPool,
    tenant: &str,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source.list_evolution_records(tenant, None, None).await?;
    builder.record_source(
        tenant,
        "evolution_records",
        rows.iter()
            .map(|row| {
                json!({
                    "id": row.id,
                    "tenant": row.tenant,
                    "record_type": row.record_type,
                    "status": row.status,
                    "created_by": row.created_by,
                    "derived_from": row.derived_from,
                    "data": json_or_string(&row.data).unwrap_or(Value::Null),
                    "timestamp": normalize_timestamp_for_value(&row.timestamp),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        let result = sqlx::query(
            "INSERT INTO evolution_records (id, tenant, record_type, status, created_by, derived_from, payload, timestamp) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (id) DO UPDATE SET tenant = EXCLUDED.tenant, record_type = EXCLUDED.record_type, \
                 status = EXCLUDED.status, created_by = EXCLUDED.created_by, derived_from = EXCLUDED.derived_from, \
                 payload = EXCLUDED.payload, timestamp = EXCLUDED.timestamp \
             WHERE evolution_records.tenant = EXCLUDED.tenant",
        )
        .bind(row.id)
        .bind(row.tenant)
        .bind(row.record_type)
        .bind(row.status)
        .bind(row.created_by)
        .bind(row.derived_from)
        .bind(json_or_string(&row.data)?)
        .bind(parse_source_timestamp(&row.timestamp)?)
        .execute(pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(anyhow!(
                "evolution record ID collision across tenants during migration"
            ));
        }
    }
    Ok(())
}

async fn migrate_blobs(
    source: &TursoEventStore,
    pool: &PgPool,
    dry_run: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let rows = source
        .list_blobs(MAX_ROWS)
        .await
        .map_err(|err| anyhow!(err))?;
    ensure_row_bound("blobs", rows.len())?;
    builder.record_source(
        GLOBAL_TENANT,
        "blobs",
        rows.iter()
            .map(|row| {
                json!({
                    "blob_key": row.blob_key,
                    "data": base64::engine::general_purpose::STANDARD.encode(&row.data),
                    "size_bytes": row.size_bytes,
                    "expires_at": row.expires_at.as_deref().map(normalize_timestamp_for_value),
                })
            })
            .collect(),
    )?;
    if dry_run {
        return Ok(());
    }
    for row in rows {
        sqlx::query(
            "INSERT INTO blobs (blob_key, data, size_bytes, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (blob_key) DO UPDATE SET data = EXCLUDED.data, size_bytes = EXCLUDED.size_bytes, \
                 created_at = EXCLUDED.created_at, expires_at = EXCLUDED.expires_at",
        )
        .bind(row.blob_key)
        .bind(row.data)
        .bind(row.size_bytes)
        .bind(parse_source_timestamp(&row.created_at)?)
        .bind(parse_optional_source_timestamp(row.expires_at.as_deref())?)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn verify_manifest_tables(
    pool: &PgPool,
    enforce: bool,
    builder: &mut ManifestBuilder,
) -> Result<()> {
    let entries = builder.tables.values().cloned().collect::<Vec<_>>();
    for entry in entries {
        let values = target_values(pool, &entry.tenant, &entry.table).await?;
        builder.record_target(&entry.tenant, &entry.table, values, enforce)?;
    }
    Ok(())
}

async fn target_values(pool: &PgPool, tenant: &str, table: &str) -> Result<Vec<Value>> {
    match table {
        "events" => target_events(pool, tenant).await,
        "snapshots" => target_snapshots(pool, tenant).await,
        "specs" => target_specs(pool, tenant).await,
        "tenant_policies" => target_tenant_policies(pool, tenant).await,
        "policies" => target_policies(pool, tenant).await,
        "tenant_constraints" => target_tenant_constraints(pool, tenant).await,
        "tenant_installed_apps" => target_installed_apps(pool, tenant).await,
        "trajectories" => target_trajectories(pool, tenant).await,
        "pending_decisions" => target_pending_decisions(pool, tenant).await,
        "wasm_modules" => target_wasm_modules(pool, tenant).await,
        "wasm_invocation_logs" => target_wasm_invocations(pool, tenant).await,
        "design_time_events" => target_design_time_events(pool, tenant).await,
        "ots_trajectories" => target_ots_trajectories(pool, tenant).await,
        "tenant_secrets" => target_secrets(pool, tenant).await,
        "policy_denial_patterns" => target_policy_denial_patterns(pool, tenant).await,
        "entity_catalog" => target_query_projections(pool, tenant).await,
        "feature_requests" => target_feature_requests(pool).await,
        "evolution_records" => target_evolution_records(pool).await,
        "blobs" => target_blobs(pool).await,
        other => Err(anyhow!("no target verifier for table {other}")),
    }
}

async fn target_events(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata \
         FROM events WHERE tenant = $1 ORDER BY entity_type, entity_id, sequence_nr",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "entity_type": row.get::<String, _>("entity_type"),
                "entity_id": row.get::<String, _>("entity_id"),
                "sequence_nr": row.get::<i64, _>("sequence_nr") as u64,
                "event_type": row.get::<String, _>("event_type"),
                "payload": row.get::<Value, _>("payload"),
                "metadata": row.get::<Value, _>("metadata"),
            })
        })
        .collect())
}

async fn target_snapshots(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, entity_id, sequence_nr, state \
         FROM snapshots WHERE tenant = $1 ORDER BY entity_type, entity_id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let state: Vec<u8> = row.get("state");
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "entity_type": row.get::<String, _>("entity_type"),
                "entity_id": row.get::<String, _>("entity_id"),
                "sequence_nr": row.get::<i64, _>("sequence_nr") as u64,
                "state": base64::engine::general_purpose::STANDARD.encode(state),
            })
        })
        .collect())
}

async fn target_specs(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, levels_passed, \
                levels_total, verification_result, content_hash, committed \
         FROM specs WHERE tenant = $1 ORDER BY entity_type",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "entity_type": row.get::<String, _>("entity_type"),
                "ioa_source": row.get::<String, _>("ioa_source"),
                "csdl_xml": row.get::<Option<String>, _>("csdl_xml"),
                "verification_status": row.get::<String, _>("verification_status"),
                "verified": row.get::<bool, _>("verified"),
                "levels_passed": row.get::<Option<i32>, _>("levels_passed"),
                "levels_total": row.get::<Option<i32>, _>("levels_total"),
                "verification_result": row.get::<Option<Value>, _>("verification_result"),
                "content_hash": row.get::<String, _>("content_hash"),
                "committed": row.get::<bool, _>("committed"),
            })
        })
        .collect())
}

async fn target_tenant_policies(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, policy_text FROM tenant_policies WHERE tenant = $1 ORDER BY tenant",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "policy_text": row.get::<String, _>("policy_text"),
            })
        })
        .collect())
}

async fn target_policies(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, policy_id, cedar_text, policy_hash, created_by, enabled \
         FROM policies WHERE tenant = $1 ORDER BY policy_id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "policy_id": row.get::<String, _>("policy_id"),
                "cedar_text": row.get::<String, _>("cedar_text"),
                "policy_hash": row.get::<String, _>("policy_hash"),
                "created_by": row.get::<String, _>("created_by"),
                "enabled": row.get::<bool, _>("enabled"),
            })
        })
        .collect())
}

async fn target_tenant_constraints(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, cross_invariants_toml, version FROM tenant_constraints WHERE tenant = $1",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "cross_invariants_toml": row.get::<String, _>("cross_invariants_toml"),
                "version": row.get::<i32, _>("version"),
            })
        })
        .collect())
}

async fn target_installed_apps(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, app_name, source_kind, app_ref, version_hash, pinned_version_hash, current_version_hash, follow_policy, \
                closure_id, registry_url, registry_tenant, app_version, bundle_digest, spec_digest, policy_digest, wasm_digest, \
                content_digest, seed_digest, status \
         FROM tenant_installed_apps WHERE tenant = $1 ORDER BY app_name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(installed_app_pg_value).collect())
}

async fn target_trajectories(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, agent_id, session_id, \
                authz_denied, denied_resource, denied_module, source, spec_governed, created_at, request_body, intent, matched_policy_ids \
         FROM trajectories WHERE tenant = $1 ORDER BY created_at, entity_type, entity_id, action",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(trajectory_pg_value).collect())
}

async fn target_pending_decisions(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, tenant, status, data FROM pending_decisions WHERE tenant = $1 ORDER BY id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "tenant": row.get::<String, _>("tenant"),
                "status": row.get::<String, _>("status"),
                "data": row.get::<Value, _>("data"),
            })
        })
        .collect())
}

async fn target_wasm_modules(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, module_name, wasm_bytes, sha256_hash, size_bytes \
         FROM wasm_modules WHERE tenant = $1 ORDER BY module_name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let bytes: Vec<u8> = row.get("wasm_bytes");
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "module_name": row.get::<String, _>("module_name"),
                "sha256_hash": row.get::<String, _>("sha256_hash"),
                "size_bytes": row.get::<i32, _>("size_bytes"),
                "wasm_bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect())
}

async fn target_wasm_invocations(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, entity_id, module_name, trigger_action, callback_action, success, error, duration_ms, created_at \
         FROM wasm_invocation_logs WHERE tenant = $1 ORDER BY created_at, entity_type, entity_id, module_name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "entity_type": row.get::<String, _>("entity_type"),
                "entity_id": row.get::<String, _>("entity_id"),
                "module_name": row.get::<String, _>("module_name"),
                "trigger_action": row.get::<String, _>("trigger_action"),
                "callback_action": row.get::<Option<String>, _>("callback_action"),
                "success": row.get::<bool, _>("success"),
                "error": row.get::<Option<String>, _>("error"),
                "duration_ms": row.get::<i64, _>("duration_ms") as u64,
                "created_at": row.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect())
}

async fn target_design_time_events(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT kind, entity_type, tenant, summary, level, passed, step_number, total_steps, created_at \
         FROM design_time_events WHERE tenant = $1 ORDER BY created_at, kind, entity_type, summary",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "kind": row.get::<String, _>("kind"),
                "entity_type": row.get::<String, _>("entity_type"),
                "tenant": row.get::<String, _>("tenant"),
                "summary": row.get::<String, _>("summary"),
                "level": row.get::<Option<String>, _>("level"),
                "passed": row.get::<Option<bool>, _>("passed"),
                "step_number": row.get::<Option<i16>, _>("step_number").map(i64::from),
                "total_steps": row.get::<Option<i16>, _>("total_steps").map(i64::from),
                "created_at": row.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect())
}

async fn target_ots_trajectories(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT trajectory_id, tenant, agent_id, COALESCE(session_id, '') AS session_id, outcome, turn_count, data, created_at \
         FROM ots_trajectories WHERE tenant = $1 ORDER BY created_at, trajectory_id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "trajectory_id": row.get::<String, _>("trajectory_id"),
                "tenant": row.get::<String, _>("tenant"),
                "agent_id": row.get::<String, _>("agent_id"),
                "session_id": row.get::<String, _>("session_id"),
                "outcome": row.get::<String, _>("outcome"),
                "turn_count": row.get::<i64, _>("turn_count"),
                "data": row.get::<Value, _>("data"),
                "created_at": row.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect())
}

async fn target_secrets(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, key_name, ciphertext, nonce FROM tenant_secrets WHERE tenant = $1 ORDER BY key_name",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let ciphertext: Vec<u8> = row.get("ciphertext");
            let nonce: Vec<u8> = row.get("nonce");
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "key_name": row.get::<String, _>("key_name"),
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(ciphertext),
                "nonce": base64::engine::general_purpose::STANDARD.encode(nonce),
            })
        })
        .collect())
}

async fn target_policy_denial_patterns(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, agent_type, action, resource_type, count, first_seen, last_seen, distinct_resource_ids_json \
         FROM policy_denial_patterns WHERE tenant = $1 ORDER BY agent_type, action, resource_type",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let agent_type = row.get::<String, _>("agent_type");
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "agent_type": if agent_type.is_empty() { Value::Null } else { Value::String(agent_type) },
                "action": row.get::<String, _>("action"),
                "resource_type": row.get::<String, _>("resource_type"),
                "count": row.get::<i64, _>("count"),
                "first_seen": row.get::<DateTime<Utc>, _>("first_seen").to_rfc3339(),
                "last_seen": row.get::<DateTime<Utc>, _>("last_seen").to_rfc3339(),
                "distinct_resource_ids_json": row.get::<Value, _>("distinct_resource_ids_json"),
            })
        })
        .collect())
}

async fn target_query_projections(pool: &PgPool, tenant: &str) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT tenant, entity_type, entity_id, status, fields, sequence_nr \
         FROM entity_catalog WHERE tenant = $1 ORDER BY entity_type, entity_id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "tenant": row.get::<String, _>("tenant"),
                "entity_type": row.get::<String, _>("entity_type"),
                "entity_id": row.get::<String, _>("entity_id"),
                "status": row.get::<String, _>("status"),
                "fields": row.get::<Value, _>("fields"),
                "sequence_nr": row.get::<i64, _>("sequence_nr") as u64,
            })
        })
        .collect())
}

async fn target_feature_requests(pool: &PgPool) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, category, description, frequency, trajectory_refs, disposition, developer_notes \
         FROM feature_requests ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "category": row.get::<String, _>("category"),
                "description": row.get::<String, _>("description"),
                "frequency": row.get::<i64, _>("frequency"),
                "trajectory_refs": row.get::<Value, _>("trajectory_refs"),
                "disposition": row.get::<String, _>("disposition"),
                "developer_notes": row.get::<Option<String>, _>("developer_notes"),
            })
        })
        .collect())
}

async fn target_evolution_records(pool: &PgPool) -> Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, tenant, record_type, status, created_by, derived_from, payload, timestamp \
         FROM evolution_records ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.get::<String, _>("id"),
                "tenant": row.get::<String, _>("tenant"),
                "record_type": row.get::<String, _>("record_type"),
                "status": row.get::<String, _>("status"),
                "created_by": row.get::<String, _>("created_by"),
                "derived_from": row.get::<Option<String>, _>("derived_from"),
                "data": row.get::<Value, _>("payload"),
                "timestamp": row.get::<DateTime<Utc>, _>("timestamp").to_rfc3339(),
            })
        })
        .collect())
}

async fn target_blobs(pool: &PgPool) -> Result<Vec<Value>> {
    let rows =
        sqlx::query("SELECT blob_key, data, size_bytes, expires_at FROM blobs ORDER BY blob_key")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let data: Vec<u8> = row.get("data");
            json!({
                "blob_key": row.get::<String, _>("blob_key"),
                "data": base64::engine::general_purpose::STANDARD.encode(data),
                "size_bytes": row.get::<i64, _>("size_bytes"),
                "expires_at": row.get::<Option<DateTime<Utc>>, _>("expires_at").map(|dt| dt.to_rfc3339()),
            })
        })
        .collect())
}

fn event_value(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    event: &PersistenceEnvelope,
) -> Result<Value> {
    Ok(json!({
        "tenant": tenant,
        "entity_type": entity_type,
        "entity_id": entity_id,
        "sequence_nr": event.sequence_nr,
        "event_type": event.event_type,
        "payload": event.payload,
        "metadata": serde_json::to_value(&event.metadata)?,
    }))
}

fn spec_value(row: &TursoSpecRow) -> Result<Value> {
    Ok(json!({
        "tenant": row.tenant,
        "entity_type": row.entity_type,
        "ioa_source": row.ioa_source,
        "csdl_xml": row.csdl_xml,
        "verification_status": row.verification_status,
        "verified": row.verified,
        "levels_passed": row.levels_passed,
        "levels_total": row.levels_total,
        "verification_result": row.verification_result.as_deref().map(json_or_string).transpose()?,
        "content_hash": row.content_hash.clone().filter(|hash| !hash.is_empty()).unwrap_or_else(|| spec_content_hash(&row.ioa_source)),
        "committed": row.committed,
    }))
}

fn installed_app_value(row: &TursoInstalledAppRow) -> Value {
    json!({
        "tenant": row.tenant_id,
        "app_name": row.app_name,
        "source_kind": row.source_kind,
        "app_ref": row.app_ref,
        "version_hash": row.version_hash,
        "pinned_version_hash": row.pinned_version_hash,
        "current_version_hash": row.current_version_hash,
        "follow_policy": row.follow_policy,
        "closure_id": row.closure_id,
        "registry_url": row.registry_url,
        "registry_tenant": row.registry_tenant,
        "app_version": row.app_version,
        "bundle_digest": row.bundle_digest,
        "spec_digest": row.spec_digest,
        "policy_digest": row.policy_digest,
        "wasm_digest": row.wasm_digest,
        "content_digest": row.content_digest,
        "seed_digest": row.seed_digest,
        "status": row.status,
    })
}

fn installed_app_pg_value(row: sqlx::postgres::PgRow) -> Value {
    json!({
        "tenant": row.get::<String, _>("tenant"),
        "app_name": row.get::<String, _>("app_name"),
        "source_kind": row.get::<String, _>("source_kind"),
        "app_ref": row.get::<String, _>("app_ref"),
        "version_hash": row.get::<String, _>("version_hash"),
        "pinned_version_hash": row.get::<String, _>("pinned_version_hash"),
        "current_version_hash": row.get::<String, _>("current_version_hash"),
        "follow_policy": row.get::<String, _>("follow_policy"),
        "closure_id": row.get::<String, _>("closure_id"),
        "registry_url": row.get::<String, _>("registry_url"),
        "registry_tenant": row.get::<String, _>("registry_tenant"),
        "app_version": row.get::<String, _>("app_version"),
        "bundle_digest": row.get::<String, _>("bundle_digest"),
        "spec_digest": row.get::<String, _>("spec_digest"),
        "policy_digest": row.get::<String, _>("policy_digest"),
        "wasm_digest": row.get::<String, _>("wasm_digest"),
        "content_digest": row.get::<String, _>("content_digest"),
        "seed_digest": row.get::<String, _>("seed_digest"),
        "status": row.get::<String, _>("status"),
    })
}

fn trajectory_value(row: &temper_store_turso::TursoTrajectoryRow) -> Result<Value> {
    Ok(json!({
        "tenant": row.tenant,
        "entity_type": row.entity_type,
        "entity_id": row.entity_id,
        "action": row.action,
        "success": row.success,
        "from_status": row.from_status,
        "to_status": row.to_status,
        "error": row.error,
        "agent_id": row.agent_id,
        "session_id": row.session_id,
        "authz_denied": row.authz_denied,
        "denied_resource": row.denied_resource,
        "denied_module": row.denied_module,
        "source": row.source,
        "spec_governed": row.spec_governed,
        "created_at": normalize_timestamp_for_value(&row.created_at),
        "request_body": row.request_body.as_deref().map(json_or_string).transpose()?,
        "intent": row.intent,
        "matched_policy_ids": row.matched_policy_ids,
    }))
}

fn trajectory_pg_value(row: sqlx::postgres::PgRow) -> Value {
    json!({
        "tenant": row.get::<String, _>("tenant"),
        "entity_type": row.get::<String, _>("entity_type"),
        "entity_id": row.get::<String, _>("entity_id"),
        "action": row.get::<String, _>("action"),
        "success": row.get::<bool, _>("success"),
        "from_status": row.get::<Option<String>, _>("from_status"),
        "to_status": row.get::<Option<String>, _>("to_status"),
        "error": row.get::<Option<String>, _>("error"),
        "agent_id": row.get::<Option<String>, _>("agent_id"),
        "session_id": row.get::<Option<String>, _>("session_id"),
        "authz_denied": row.get::<Option<bool>, _>("authz_denied"),
        "denied_resource": row.get::<Option<String>, _>("denied_resource"),
        "denied_module": row.get::<Option<String>, _>("denied_module"),
        "source": row.get::<Option<String>, _>("source"),
        "spec_governed": row.get::<Option<bool>, _>("spec_governed"),
        "created_at": row.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
        "request_body": row.get::<Option<Value>, _>("request_body"),
        "intent": row.get::<Option<String>, _>("intent"),
        "matched_policy_ids": row.get::<Option<Value>, _>("matched_policy_ids"),
    })
}

fn feature_request_value(row: &FeatureRequestRow) -> Result<Value> {
    Ok(json!({
        "id": row.id,
        "category": row.category,
        "description": row.description,
        "frequency": row.frequency,
        "trajectory_refs": json_or_string(&row.trajectory_refs)?,
        "disposition": row.disposition,
        "developer_notes": row.developer_notes,
    }))
}

fn decision_value(data: &str) -> Result<Value> {
    let value = json_or_string(data)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("migration-{}", short_checksum(data.as_bytes())));
    let tenant = value
        .get("tenant")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending")
        .to_string();
    Ok(json!({
        "id": id,
        "tenant": tenant,
        "status": status,
        "data": value,
    }))
}

fn json_or_string(data: &str) -> Result<Value> {
    match serde_json::from_str::<Value>(data) {
        Ok(value) => Ok(value),
        Err(_) => Ok(Value::String(data.to_string())),
    }
}

fn checksum_values(values: Vec<Value>) -> Result<String> {
    let mut rendered = values
        .into_iter()
        .map(|value| serde_json::to_string(&normalize_json(value)))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rendered.sort();
    let mut hasher = Sha256::new();
    for value in rendered {
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(normalize_json).collect()),
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, normalize_json(value.clone()));
                }
            }
            Value::Object(sorted)
        }
        value => value,
    }
}

fn short_checksum(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}

fn parse_source_timestamp(value: &str) -> Result<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(DateTime::from_naive_utc_and_offset(parsed, Utc));
    }
    Err(anyhow!("unsupported timestamp format {value:?}"))
}

fn parse_source_timestamp_or_now(value: &str) -> DateTime<Utc> {
    parse_source_timestamp(value).unwrap_or_else(|_| Utc::now())
}

fn parse_optional_source_timestamp(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(parse_source_timestamp)
        .transpose()
}

fn normalize_timestamp_for_value(value: &str) -> String {
    parse_source_timestamp(value)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|_| value.to_string())
}

fn required_value(value: Option<String>, env_name: &str, flag_name: &str) -> Result<String> {
    value
        .or_else(|| std::env::var(env_name).ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{flag_name} or {env_name} must be set"))
}

fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some(at_idx) = rest.find('@') else {
        return url.to_string();
    };
    let creds = &rest[..at_idx];
    let host_and_path = &rest[at_idx + 1..];
    if let Some((user, _password)) = creds.split_once(':') {
        format!("{scheme}://{user}:***@{host_and_path}")
    } else {
        format!("{scheme}://***@{host_and_path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn smoke_migration_copies_events_snapshots_specs_projections_and_blobs_when_database_url_set()
     {
        let Ok(database_url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping migration smoke test: DATABASE_URL is not set");
            return;
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let source_path = temp.path().join("source.db");
        let source_url = format!("file:{}", source_path.display());
        let source = TursoEventStore::new(&source_url, None)
            .await
            .expect("source store");
        let tenant = format!("migration-smoke-{}", uuid::Uuid::new_v4());
        let persistence_id = format!("{tenant}:SmokeEntity:smoke-1");
        let envelope = PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Created".to_string(),
            payload: json!({ "id": "smoke-1", "kind": "migration-smoke" }),
            metadata: temper_runtime::persistence::EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: Utc::now(),
                actor_id: "migration-smoke".to_string(),
            },
        };
        source
            .append(&persistence_id, 0, &[envelope])
            .await
            .expect("append source event");
        source
            .save_snapshot(&persistence_id, 1, br#"{"Status":"Ready"}"#)
            .await
            .expect("save source snapshot");
        source
            .upsert_spec(
                &tenant,
                "SmokeEntity",
                "[entity]\nname = \"SmokeEntity\"\n",
                "<Schema />",
                "hash-smoke",
            )
            .await
            .expect("upsert source spec");
        source
            .commit_specs(&tenant)
            .await
            .expect("commit source spec");
        source
            .upsert_query_projection(
                &tenant,
                "SmokeEntity",
                "smoke-1",
                "Ready",
                &json!({ "kind": "migration-smoke", "has_content": true }),
                1,
            )
            .await
            .expect("upsert source projection");
        let blob_key = format!("migration-smoke/{}", uuid::Uuid::new_v4());
        source
            .put_blob(&blob_key, b"migration-smoke-blob")
            .await
            .expect("put source blob");

        run(MigrationOptions {
            tenant: tenant.clone(),
            dry_run: false,
            verify: false,
            from_snapshot: true,
            manifest_path: temp.path().join("manifest.json"),
            turso_url: Some(source_url),
            turso_auth_token: None,
            database_url: Some(database_url.clone()),
        })
        .await
        .expect("run migration");

        let pool = PgPool::connect(&database_url).await.expect("target pool");
        let event_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM events WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .expect("count events");
        assert_eq!(event_count, 1);
        let snapshot_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM snapshots WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .expect("count snapshots");
        assert_eq!(snapshot_count, 1);
        let projection_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM entity_catalog WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .expect("count projections");
        assert_eq!(projection_count, 1);
        let spec_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM specs WHERE tenant = $1")
                .bind(&tenant)
                .fetch_one(&pool)
                .await
                .expect("count specs");
        assert_eq!(spec_count, 1);
        let blob_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::bigint FROM blobs WHERE blob_key = $1")
                .bind(&blob_key)
                .fetch_one(&pool)
                .await
                .expect("count blobs");
        assert_eq!(blob_count, 1);

        cleanup_smoke_rows(&pool, &tenant, &blob_key)
            .await
            .expect("cleanup smoke rows");
    }

    async fn cleanup_smoke_rows(pool: &PgPool, tenant: &str, blob_key: &str) -> Result<()> {
        for table in [
            "events",
            "snapshots",
            "specs",
            "tenant_policies",
            "policies",
            "tenant_constraints",
            "tenant_installed_apps",
            "trajectories",
            "pending_decisions",
            "wasm_modules",
            "wasm_invocation_logs",
            "design_time_events",
            "ots_trajectories",
            "tenant_secrets",
            "policy_denial_patterns",
            // entity_field_index has no migrate_* function (it is derived:
            // upsert_query_projection rebuilds it from entity_catalog.fields),
            // but the smoke migration populates it as a side effect, so the
            // cleanup must still delete the derived rows.
            "entity_field_index",
            "entity_catalog",
        ] {
            let sql = format!("DELETE FROM {table} WHERE tenant = $1");
            sqlx::query(&sql).bind(tenant).execute(pool).await?;
        }
        sqlx::query("DELETE FROM blobs WHERE blob_key = $1")
            .bind(blob_key)
            .execute(pool)
            .await?;
        Ok(())
    }

    #[test]
    fn parses_sqlite_and_rfc3339_source_timestamps() {
        assert_eq!(
            parse_source_timestamp("2026-04-28 12:34:56")
                .unwrap()
                .to_rfc3339(),
            "2026-04-28T12:34:56+00:00"
        );
        assert_eq!(
            parse_source_timestamp("2026-04-28T12:34:56Z")
                .unwrap()
                .to_rfc3339(),
            "2026-04-28T12:34:56+00:00"
        );
    }

    #[test]
    fn decision_value_extracts_stable_identity() {
        let value = decision_value(
            r#"{"id":"PD-1","tenant":"acme","status":"approved","resource_id":"secret"}"#,
        )
        .unwrap();
        assert_eq!(value.get("id").and_then(Value::as_str), Some("PD-1"));
        assert_eq!(value.get("tenant").and_then(Value::as_str), Some("acme"));
        assert_eq!(
            value.get("status").and_then(Value::as_str),
            Some("approved")
        );
    }

    #[test]
    fn checksum_values_is_order_and_object_key_stable() {
        let a = checksum_values(vec![json!({"b": 2, "a": 1}), json!({"x": true})]).unwrap();
        let b = checksum_values(vec![json!({"x": true}), json!({"a": 1, "b": 2})]).unwrap();
        assert_eq!(a, b);
    }
}
