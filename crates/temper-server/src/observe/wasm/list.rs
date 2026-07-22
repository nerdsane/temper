use axum::extract::{Query, State};

use super::*;

/// GET /observe/wasm/modules — list all modules (with stats).
///
/// Admin/System principals see all tenants; others are scoped to `X-Tenant-Id`.
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/modules"))]
pub async fn handle_list_wasm_modules(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let stores = state.collect_all_metadata_stores().await;
    let mut guarded_tenants = BTreeSet::new();
    let mut _all_generation_guards = Vec::new();
    let _tenant_generation_guard;
    if let Some(tenant) = tenant_scope.as_ref() {
        _tenant_generation_guard = Some(begin_wasm_generation_read(&state, tenant).await?);
    } else {
        _tenant_generation_guard = None;
        guarded_tenants = known_wasm_tenants(&state)?;
        guarded_tenants.insert(wasm_authorization_tenant(&headers));
        for store in &stores {
            if let Ok(rows) = store.load_recent_wasm_invocations(10_000).await {
                guarded_tenants.extend(rows.into_iter().map(|row| row.tenant));
            }
        }
        _all_generation_guards = begin_all_wasm_generation_reads(&state, &guarded_tenants).await?;
    }
    require_observe_auth(&state, &headers, "read_wasm", "WasmModule")?;

    // Collect invocation stats via fan-out across all tenant stores.
    let invocation_stats: BTreeMap<(String, String), (usize, usize, Option<String>)> = {
        let mut stats = BTreeMap::new();
        for store in &stores {
            if let Ok(rows) = store.load_recent_wasm_invocations(10_000).await {
                for row in rows {
                    if tenant_scope
                        .as_ref()
                        .is_some_and(|tenant| tenant.as_str() != row.tenant)
                    {
                        continue;
                    }
                    if tenant_scope.is_none() && !guarded_tenants.contains(&row.tenant) {
                        return Err(StatusCode::SERVICE_UNAVAILABLE);
                    }
                    let module = (row.tenant.clone(), row.module_name.clone());
                    let success = row.success;
                    let ts = Some(row.created_at.clone());
                    let (total, s_count, last_ts) = stats.entry(module).or_insert((0, 0, None));
                    *total += 1;
                    if success {
                        *s_count += 1;
                    }
                    if ts.is_some() {
                        *last_ts = ts;
                    }
                }
            }
        }
        stats
    };

    let modules: Vec<WasmModuleListEntry> = {
        let wasm_reg = state.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
        let tenant_modules = wasm_reg.all_modules();
        if tenant_scope.is_none()
            && tenant_modules
                .iter()
                .any(|(tenant, _, _)| !guarded_tenants.contains(*tenant))
        {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }

        let make_entry = |tenant: &str, name: &str, hash: &str| {
            let cached = state.wasm_engine.is_cached(hash);
            let (total_invocations, success_count, last_invoked_at) = invocation_stats
                .get(&(tenant.to_string(), name.to_string()))
                .cloned()
                .unwrap_or((0, 0, None));
            let success_rate = if total_invocations > 0 {
                success_count as f64 / total_invocations as f64
            } else {
                0.0
            };
            WasmModuleListEntry {
                tenant: tenant.to_string(),
                module_name: name.to_string(),
                sha256_hash: hash.to_string(),
                cached,
                total_invocations,
                success_count,
                success_rate,
                last_invoked_at,
            }
        };

        let mut entries: Vec<WasmModuleListEntry> = tenant_modules
            .into_iter()
            .filter(|(tenant, _, _)| {
                tenant_scope
                    .as_ref()
                    .is_none_or(|scope| scope.as_str() == *tenant)
            })
            .map(|(tenant, name, hash)| make_entry(tenant, name, hash))
            .collect();

        // Include built-in modules (visible to all tenants, no tenant scope filter).
        for (name, hash) in wasm_reg.all_builtins() {
            entries.push(make_entry("builtin", name, hash));
        }

        entries
    };

    let total = modules.len();
    Ok(Json(serde_json::json!({
        "modules": modules,
        "total": total,
    })))
}

/// GET /observe/wasm/invocations — query WASM invocation history.
#[instrument(skip_all, fields(otel.name = "GET /observe/wasm/invocations"))]
pub async fn handle_list_wasm_invocations(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<InvocationQueryParams>,
) -> Result<Json<WasmInvocationResponse>, StatusCode> {
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let limit = params.limit.unwrap_or(100).min(10_000);
    let stores = state.collect_all_metadata_stores().await;
    let mut guarded_tenants = BTreeSet::new();
    let mut _all_generation_guards = Vec::new();
    let _tenant_generation_guard;
    if let Some(tenant) = tenant_scope.as_ref() {
        _tenant_generation_guard = Some(begin_wasm_generation_read(&state, tenant).await?);
    } else {
        _tenant_generation_guard = None;
        guarded_tenants = known_wasm_tenants(&state)?;
        guarded_tenants.insert(wasm_authorization_tenant(&headers));
        for store in &stores {
            if let Ok(rows) = store.load_recent_wasm_invocations(limit as i64).await {
                guarded_tenants.extend(rows.into_iter().map(|row| row.tenant));
            }
        }
        _all_generation_guards = begin_all_wasm_generation_reads(&state, &guarded_tenants).await?;
    }
    require_observe_auth(&state, &headers, "read_wasm", "WasmModule")?;

    let mut all_filtered: Vec<serde_json::Value> = Vec::new();
    for store in &stores {
        match store.load_recent_wasm_invocations(limit as i64).await {
            Ok(rows) => {
                if tenant_scope.is_none()
                    && rows
                        .iter()
                        .any(|entry| !guarded_tenants.contains(&entry.tenant))
                {
                    return Err(StatusCode::SERVICE_UNAVAILABLE);
                }
                let filtered: Vec<serde_json::Value> = rows
                    .into_iter()
                    .filter(|e| {
                        if tenant_scope
                            .as_ref()
                            .is_some_and(|tenant| tenant.as_str() != e.tenant)
                        {
                            return false;
                        }
                        if let Some(ref mn) = params.module_name
                            && e.module_name != *mn
                        {
                            return false;
                        }
                        if let Some(s) = params.success
                            && e.success != s
                        {
                            return false;
                        }
                        true
                    })
                    .map(|e| serde_json::to_value(&e).unwrap_or_default())
                    .collect();
                all_filtered.extend(filtered);
            }
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), "failed to query WASM invocations");
            }
        }
    }

    let total = all_filtered.len();
    Ok(Json(WasmInvocationResponse {
        invocations: all_filtered,
        total,
    }))
}
