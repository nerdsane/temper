//! In-memory routing table for ADR-0069 `HttpEndpoint` entities.
//!
//! The kernel's axum router consults this table as a fallback (after
//! built-in routes). On each request, the table does longest-prefix
//! match on the request path against all `Active` HttpEndpoint rows
//! registered for the tenant, extracts templated path parameters,
//! and returns the matched route. The dispatcher then opens an
//! inbound exchange (ADR-0057 Phase 2) and invokes the bound WASM
//! integration. Some endpoints may also declare an action bridge:
//! the WASM adapter returns typed parameters, and the kernel dispatches
//! the statically configured Temper action.
//!
//! Discipline (per ADR-0069):
//!   * Longest-prefix match wins; ties on length are rejected at
//!     `Create` time by a cross-field invariant.
//!   * Built-in namespaces (`/tdata`, `/webhooks`, `/_admin`,
//!     `/observe`, `/api`, `/_internal`) are reserved; the spec's
//!     invariants block them at write time.
//!   * Methods are matched case-insensitively against the row's
//!     comma-separated Methods column.
//!   * `Paused` / `Deleted` endpoints never match.

use std::collections::BTreeMap;
use std::time::Duration;

use temper_runtime::tenant::TenantId;
use tokio::sync::RwLock;

const ACTOR_ASK_TIMEOUT: Duration = Duration::from_millis(500);

/// One row of the table. Derived from an `HttpEndpoint` entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpEndpointRoute {
    /// Entity Id — `he-<uuid>` per the spec. Kept for telemetry /
    /// Cedar resource binding.
    pub id: String,
    /// Path prefix to match. Starts with `/`, may contain
    /// `{param}` segments for parameter extraction.
    pub path_prefix: String,
    /// Uppercase HTTP methods this route accepts.
    pub methods: Vec<String>,
    /// Name of the WASM integration to dispatch to.
    pub integration_module: String,
    /// If true, the kernel resolves a Principal before dispatch.
    pub requires_auth: bool,
    /// Hard cap on invocation wall time (seconds).
    pub timeout_secs: u32,
    /// Optional instruction budget for the endpoint adapter.
    pub max_fuel: Option<u64>,
    /// Optional linear-memory budget for the endpoint adapter.
    pub max_memory: Option<usize>,
    /// Optional host HTTP response ceiling for the endpoint adapter.
    pub max_response_bytes: Option<usize>,
    /// Optional kernel-owned bridge from the adapter result to a
    /// spec-defined action. The WASM module supplies parameters only;
    /// the endpoint row supplies the target action and entity template.
    pub action_bridge: Option<HttpActionBridge>,
}

/// Kernel-owned bridge from an HttpEndpoint adapter result to a
/// spec-defined Temper action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpActionBridge {
    /// Entity type to dispatch on, e.g. `Repository`.
    pub entity_type: String,
    /// Entity id template, e.g. `rp-{owner}-{repo}`. Placeholders come
    /// from the route's extracted path params.
    pub entity_id_template: String,
    /// Spec-defined action name, e.g. `IngestPack`.
    pub action: String,
    /// Response renderer for the foreign protocol. `git-receive-pack`
    /// is the first supported renderer.
    pub response: String,
}

/// Successful match: the route plus extracted path params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedRoute {
    pub route: HttpEndpointRoute,
    /// Path parameters extracted from `{name}` segments, keyed on
    /// the param name (without braces).
    pub params: BTreeMap<String, String>,
}

/// In-memory route table. One instance per tenant in the kernel.
/// Rebuilt on entity-state change events; lookups take an async
/// read lock so concurrent requests don't serialize against
/// reconciler updates.
pub struct HttpEndpointTable {
    rows: RwLock<Vec<HttpEndpointRoute>>,
}

impl HttpEndpointTable {
    pub fn new() -> Self {
        Self {
            rows: RwLock::new(Vec::new()),
        }
    }

    /// Replace the full set of rows. Called by the reconciler when
    /// any `HttpEndpoint` entity in this tenant transitions state.
    pub async fn replace(&self, rows: Vec<HttpEndpointRoute>) {
        let mut guard = self.rows.write().await;
        *guard = rows;
    }

    /// Number of registered rows — for metrics and tests.
    pub async fn len(&self) -> usize {
        self.rows.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.rows.read().await.is_empty()
    }

    /// Longest-prefix match against the given (method, path).
    /// Returns None if no registered row accepts the method AND
    /// matches the path. Method matching is case-insensitive.
    pub async fn match_request(&self, method: &str, path: &str) -> Option<MatchedRoute> {
        let method_u = method.to_uppercase();
        let rows = self.rows.read().await;

        let mut best: Option<(usize, MatchedRoute)> = None;
        for row in rows.iter() {
            if !row.methods.iter().any(|m| m == &method_u) {
                continue;
            }
            let Some(params) = match_path_prefix(&row.path_prefix, path) else {
                continue;
            };
            let score = row.path_prefix.len();
            let candidate = MatchedRoute {
                route: row.clone(),
                params,
            };
            match &best {
                Some((cur, _)) if *cur >= score => {}
                _ => best = Some((score, candidate)),
            }
        }
        best.map(|(_, m)| m)
    }
}

impl Default for HttpEndpointTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-tenant registry of route tables. Dispatcher consults this
/// first to pick the right tenant's table.
pub struct HttpEndpointTables {
    by_tenant: RwLock<BTreeMap<TenantId, std::sync::Arc<HttpEndpointTable>>>,
}

impl HttpEndpointTables {
    pub fn new() -> Self {
        Self {
            by_tenant: RwLock::new(BTreeMap::new()),
        }
    }

    /// Get (or lazily create) the table for a tenant.
    pub async fn table_for(&self, tenant: &TenantId) -> std::sync::Arc<HttpEndpointTable> {
        {
            let guard = self.by_tenant.read().await;
            if let Some(table) = guard.get(tenant) {
                return table.clone();
            }
        }
        let mut guard = self.by_tenant.write().await;
        guard
            .entry(tenant.clone())
            .or_insert_with(|| std::sync::Arc::new(HttpEndpointTable::new()))
            .clone()
    }

    pub async fn tenant_count(&self) -> usize {
        self.by_tenant.read().await.len()
    }
}

impl Default for HttpEndpointTables {
    fn default() -> Self {
        Self::new()
    }
}

/// Rebuild the route table for `tenant` by enumerating every
/// `HttpEndpoint` row that the tenant owns, asking each entity
/// actor for its current state, and projecting Active rows into
/// routes. Called on boot and on every HttpEndpoint state change
/// (see [`spawn_reconciler`]).
///
/// Fails silently on per-row problems (actor unreachable, state
/// parse failure, etc.): a single malformed row should not sink
/// the rest of the table. Metrics + warn-level logs cover
/// visibility.
pub async fn rebuild_tenant_table(state: &crate::state::ServerState, tenant: &TenantId) {
    use crate::entity_actor::types::{EntityMsg, EntityResponse};

    // On fresh pod boots the in-memory entity_index is lazily
    // populated — we need to prime it from the event store or
    // rebuild sees 0 rows even when there ARE active HttpEndpoint
    // rows in durable storage. `list_entity_ids_lazy` handles the
    // populate-if-empty path.
    let ids: Vec<String> = state.list_entity_ids_lazy(tenant, "HttpEndpoint").await;

    // Force-materialise each actor from its snapshot/event log so
    // the subsequent ask(GetState) finds them in the registry.
    // Actors materialise lazily on OData reads; for boot-time
    // table priming we eagerly spawn since the router fallback
    // can't afford to 404 on the first request after restart.
    for entity_id in &ids {
        let _ = state.get_or_spawn_tenant_actor_with_fields(
            tenant,
            "HttpEndpoint",
            entity_id,
            serde_json::json!({}),
        );
    }

    let mut routes: Vec<HttpEndpointRoute> = Vec::with_capacity(ids.len());
    for entity_id in ids {
        let actor_key = format!("{}:HttpEndpoint:{}", tenant.as_str(), entity_id);
        let actor_ref = {
            let Ok(registry) = state.actor_registry.read() else {
                continue;
            };
            registry.get(&actor_key).cloned()
        };
        let Some(actor_ref) = actor_ref else {
            continue; // actor passivated — will rematerialize on next request
        };
        let response: EntityResponse =
            match actor_ref.ask(EntityMsg::GetState, ACTOR_ASK_TIMEOUT).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        tenant = %tenant.as_str(),
                        entity_id,
                        error = %e,
                        "HttpEndpoint actor ask failed during rebuild"
                    );
                    continue;
                }
            };
        if response.state.status != "Active" {
            continue;
        }
        if let Some(route) = route_from_entity_fields(&entity_id, &response.state.fields) {
            routes.push(route);
        } else {
            tracing::warn!(
                tenant = %tenant.as_str(),
                entity_id,
                status = %response.state.status,
                "HttpEndpoint row failed projection — skipped"
            );
        }
    }

    let table = state.http_endpoint_tables.table_for(tenant).await;
    let count = routes.len();
    table.replace(routes).await;
    tracing::info!(
        tenant = %tenant.as_str(),
        route_count = count,
        "HttpEndpoint route table rebuilt"
    );
}

/// Spawn a long-running task that watches entity state changes and
/// rebuilds the tenant's route table whenever an HttpEndpoint row
/// transitions. Must be called once per server instance, after
/// `ServerState::new` but before `axum::serve`.
///
/// Rebuild semantics are eager: any HttpEndpoint-typed transition
/// (Create, Pause, Resume, Delete) triggers a full re-enumeration.
/// Cost is O(active_rows) per change, acceptable at our expected
/// row count (O(hundreds) per tenant per ADR-0069).
pub fn spawn_reconciler(state: crate::state::ServerState) {
    let mut rx = state.event_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(change) if change.entity_type == "HttpEndpoint" => {
                    let tenant = TenantId::new(&change.tenant);
                    rebuild_tenant_table(&state, &tenant).await;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "HttpEndpoint reconciler lagged; rebuilding all tenant tables as a recovery"
                    );
                    let tenants: Vec<TenantId> = {
                        let Ok(reg) = state.registry.read() else {
                            continue;
                        };
                        reg.tenant_ids().iter().map(|t| (*t).clone()).collect()
                    };
                    for tenant in tenants {
                        rebuild_tenant_table(&state, &tenant).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("HttpEndpoint reconciler stopping (broadcast closed)");
                    return;
                }
            }
        }
    });
}

/// Project a raw `fields` JSON object from an `HttpEndpoint`
/// entity row into a typed [`HttpEndpointRoute`]. Returns `None`
/// if required fields are missing or malformed — caller treats
/// that as "skip this row" in the reconciler path.
pub fn route_from_entity_fields(id: &str, fields: &serde_json::Value) -> Option<HttpEndpointRoute> {
    let obj = fields.as_object()?;
    let path_prefix = obj.get("PathPrefix")?.as_str()?.to_string();
    let methods_raw = obj.get("Methods")?.as_str()?;
    let methods: Vec<String> = methods_raw
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if methods.is_empty() {
        return None;
    }
    let integration_module = obj.get("IntegrationModule")?.as_str()?.to_string();
    if integration_module.is_empty() {
        return None;
    }
    let requires_auth = obj
        .get("RequiresAuth")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let timeout_secs = obj
        .get("TimeoutSecs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60);
    let git_pack_defaults =
        integration_module == "git_receive_pack" || integration_module == "git_upload_pack";
    let max_fuel = obj
        .get("MaxFuel")
        .and_then(|v| v.as_u64())
        .or_else(|| git_pack_defaults.then_some(20_000_000_000));
    let max_memory = obj
        .get("MaxMemory")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())
        .or_else(|| git_pack_defaults.then_some(512 * 1024 * 1024));
    let max_response_bytes = obj
        .get("MaxResponseBytes")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())
        .or_else(|| git_pack_defaults.then_some(128 * 1024 * 1024));
    let action_bridge = optional_action_bridge(obj)?;
    Some(HttpEndpointRoute {
        id: id.to_string(),
        path_prefix,
        methods,
        integration_module,
        requires_auth,
        timeout_secs: timeout_secs.min(u32::MAX as u64) as u32,
        max_fuel,
        max_memory,
        max_response_bytes,
        action_bridge,
    })
}

fn optional_action_bridge(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<Option<HttpActionBridge>> {
    let entity_type = optional_string(obj, "ActionBridgeEntityType");
    let entity_id_template = optional_string(obj, "ActionBridgeEntityId");
    let action = optional_string(obj, "ActionBridgeAction");
    let response = optional_string(obj, "ActionBridgeResponse");

    if entity_type.is_none()
        && entity_id_template.is_none()
        && action.is_none()
        && response.is_none()
    {
        return Some(None);
    }

    Some(Some(HttpActionBridge {
        entity_type: entity_type?,
        entity_id_template: entity_id_template?,
        action: action?,
        response: response?,
    }))
}

fn optional_string(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Match `path` against `template`. Template segments of the form
/// `{name}` match any single path segment (no embedded `/`). Extra
/// path segments beyond the template are ignored — callers get the
/// tail by reconstructing from `path`.
///
/// Returns None if the path doesn't start with the template, or if
/// any `{name}` segment is empty.
fn match_path_prefix(template: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let mut params = BTreeMap::new();
    let mut template_segments = template.split('/');
    let mut path_segments = path.split('/');

    loop {
        let (t_seg, p_seg) = (template_segments.next(), path_segments.next());
        match (t_seg, p_seg) {
            (None, _) => return Some(params),
            (Some(""), Some("")) => continue, // leading `/` aligns
            (Some(t), Some(p)) => match match_segment(t, p) {
                Some(extracted) => {
                    for (k, v) in extracted {
                        params.insert(k, v);
                    }
                }
                None => return None,
            },
            (Some(_), None) => return None,
        }
    }
}

/// Match a single template segment against a single path segment.
/// Returns the extracted `{name}` params on success, `None` on
/// mismatch.
///
/// Supports two shapes:
///   1. Pure literal: `repos` matches only `repos`.
///   2. Templated: exactly one `{name}` placeholder, with optional
///      literal prefix + suffix. `{owner}` matches any non-empty
///      segment. `{repo}.git` matches segments ending in `.git`, with
///      the prefix captured as `repo`. `v{ver}` matches `v1`, `v2.0`,
///      etc.
fn match_segment(template: &str, path: &str) -> Option<Vec<(String, String)>> {
    if !template.contains('{') {
        return if template == path {
            Some(Vec::new())
        } else {
            None
        };
    }
    // Reject multi-placeholder segments. If the template has more
    // than one `{`, callers should register multiple sibling prefixes
    // or split the segment differently.
    if template.matches('{').count() > 1 {
        return None;
    }
    let brace_open = template.find('{')?;
    let brace_close = template.find('}')?;
    if brace_close <= brace_open {
        return None;
    }
    let prefix = &template[..brace_open];
    let name = &template[brace_open + 1..brace_close];
    let suffix = &template[brace_close + 1..];
    if !path.starts_with(prefix) || !path.ends_with(suffix) {
        return None;
    }
    if path.len() < prefix.len() + suffix.len() {
        return None;
    }
    let value = &path[prefix.len()..path.len() - suffix.len()];
    if value.is_empty() {
        return None;
    }
    Some(vec![(name.to_string(), value.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(
        id: &str,
        path_prefix: &str,
        methods: &[&str],
        integration_module: &str,
    ) -> HttpEndpointRoute {
        HttpEndpointRoute {
            id: id.to_string(),
            path_prefix: path_prefix.to_string(),
            methods: methods.iter().map(|m| m.to_uppercase()).collect(),
            integration_module: integration_module.to_string(),
            requires_auth: false,
            timeout_secs: 60,
            max_fuel: None,
            max_memory: None,
            max_response_bytes: None,
            action_bridge: None,
        }
    }

    #[tokio::test]
    async fn empty_table_matches_nothing() {
        let table = HttpEndpointTable::new();
        assert!(table.match_request("GET", "/anything").await.is_none());
    }

    #[tokio::test]
    async fn exact_path_match() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route("he-1", "/hello", &["GET"], "hello_handler")])
            .await;
        let m = table.match_request("GET", "/hello").await.unwrap();
        assert_eq!(m.route.integration_module, "hello_handler");
        assert!(m.params.is_empty());
    }

    #[tokio::test]
    async fn templated_segment_extracts_param() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/repos/{owner}/{repo}",
                &["GET"],
                "repos",
            )])
            .await;
        let m = table
            .match_request("GET", "/repos/acme/widgets")
            .await
            .unwrap();
        assert_eq!(m.params.get("owner").unwrap(), "acme");
        assert_eq!(m.params.get("repo").unwrap(), "widgets");
    }

    #[tokio::test]
    async fn longest_prefix_wins_over_shorter() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![
                route("short", "/repos", &["GET"], "list"),
                route("long", "/repos/{owner}/{repo}", &["GET"], "show"),
            ])
            .await;
        let m = table.match_request("GET", "/repos/acme/widgets").await;
        assert_eq!(m.unwrap().route.integration_module, "show");
    }

    #[tokio::test]
    async fn method_mismatch_rejects() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route("he-1", "/refs", &["GET"], "refs_read")])
            .await;
        assert!(table.match_request("POST", "/refs").await.is_none());
    }

    #[tokio::test]
    async fn method_match_case_insensitive() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route("he-1", "/refs", &["POST"], "refs_write")])
            .await;
        assert!(table.match_request("post", "/refs").await.is_some());
    }

    #[tokio::test]
    async fn tables_lazy_create_per_tenant() {
        let tables = HttpEndpointTables::new();
        let t1 = TenantId::new("tenant-a");
        let t2 = TenantId::new("tenant-b");
        let table_a = tables.table_for(&t1).await;
        let table_a_again = tables.table_for(&t1).await;
        assert!(std::sync::Arc::ptr_eq(&table_a, &table_a_again));
        let table_b = tables.table_for(&t2).await;
        assert!(!std::sync::Arc::ptr_eq(&table_a, &table_b));
        assert_eq!(tables.tenant_count().await, 2);
    }

    #[tokio::test]
    async fn empty_param_segment_rejects() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route("he-1", "/{owner}/repos", &["GET"], "h")])
            .await;
        assert!(table.match_request("GET", "//repos").await.is_none());
    }

    #[tokio::test]
    async fn trailing_path_segments_allowed_under_prefix() {
        // Template ends at {repo}; extra segments (`info/refs`) are
        // ignored for matching and available to the dispatcher via
        // `path` reconstruction if needed.
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/repos/{owner}/{repo}",
                &["GET"],
                "git_info_refs",
            )])
            .await;
        let m = table
            .match_request("GET", "/repos/acme/widgets/info/refs")
            .await
            .unwrap();
        assert_eq!(m.params.get("owner").unwrap(), "acme");
        assert_eq!(m.params.get("repo").unwrap(), "widgets");
    }

    #[tokio::test]
    async fn route_from_entity_fields_projects_required() {
        let fields = serde_json::json!({
            "PathPrefix": "/repos/{owner}/{repo}.git/info/refs",
            "Methods": "GET",
            "IntegrationModule": "git_upload_pack",
            "RequiresAuth": true,
            "TimeoutSecs": 120,
            "MaxFuel": 20000000000u64,
            "MaxMemory": 536870912u64,
            "MaxResponseBytes": 134217728u64,
        });
        let r = route_from_entity_fields("he-abc", &fields).unwrap();
        assert_eq!(r.id, "he-abc");
        assert_eq!(r.methods, vec!["GET".to_string()]);
        assert_eq!(r.integration_module, "git_upload_pack");
        assert!(r.requires_auth);
        assert_eq!(r.timeout_secs, 120);
        assert_eq!(r.max_fuel, Some(20_000_000_000));
        assert_eq!(r.max_memory, Some(536_870_912));
        assert_eq!(r.max_response_bytes, Some(134_217_728));
        assert!(r.action_bridge.is_none());
    }

    #[tokio::test]
    async fn route_from_entity_fields_projects_action_bridge() {
        let fields = serde_json::json!({
            "PathPrefix": "/{owner}/{repo}.git/git-receive-pack",
            "Methods": "POST",
            "IntegrationModule": "git_receive_pack",
            "RequiresAuth": false,
            "TimeoutSecs": 300,
            "ActionBridgeEntityType": "Repository",
            "ActionBridgeEntityId": "rp-{owner}-{repo}",
            "ActionBridgeAction": "IngestPack",
            "ActionBridgeResponse": "git-receive-pack",
        });
        let r = route_from_entity_fields("he-receive", &fields).unwrap();
        assert_eq!(r.max_fuel, Some(20_000_000_000));
        assert_eq!(r.max_memory, Some(512 * 1024 * 1024));
        assert_eq!(r.max_response_bytes, Some(128 * 1024 * 1024));
        let bridge = r.action_bridge.unwrap();
        assert_eq!(bridge.entity_type, "Repository");
        assert_eq!(bridge.entity_id_template, "rp-{owner}-{repo}");
        assert_eq!(bridge.action, "IngestPack");
        assert_eq!(bridge.response, "git-receive-pack");
    }

    #[tokio::test]
    async fn route_from_entity_fields_rejects_partial_action_bridge() {
        let fields = serde_json::json!({
            "PathPrefix": "/x",
            "Methods": "POST",
            "IntegrationModule": "handler",
            "ActionBridgeEntityType": "Repository",
        });
        assert!(route_from_entity_fields("he-partial", &fields).is_none());
    }

    #[tokio::test]
    async fn route_from_entity_fields_splits_comma_methods() {
        let fields = serde_json::json!({
            "PathPrefix": "/api/v3/repos/{owner}/{repo}/pulls",
            "Methods": "GET, POST, PATCH",
            "IntegrationModule": "github_rest_pulls",
        });
        let r = route_from_entity_fields("he-x", &fields).unwrap();
        assert_eq!(
            r.methods,
            vec!["GET".to_string(), "POST".to_string(), "PATCH".to_string()]
        );
        // Missing optional fields defaulted.
        assert!(r.requires_auth);
        assert_eq!(r.timeout_secs, 60);
    }

    #[tokio::test]
    async fn route_from_entity_fields_rejects_empty_methods() {
        let fields = serde_json::json!({
            "PathPrefix": "/x",
            "Methods": "",
            "IntegrationModule": "h",
        });
        assert!(route_from_entity_fields("he-y", &fields).is_none());
    }

    #[tokio::test]
    async fn route_from_entity_fields_rejects_missing_integration() {
        let fields = serde_json::json!({
            "PathPrefix": "/x",
            "Methods": "GET",
        });
        assert!(route_from_entity_fields("he-y", &fields).is_none());
    }

    #[tokio::test]
    async fn templated_segment_with_suffix() {
        // `{repo}.git` matches `widgets.git`; `repo` captures `widgets`.
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/{owner}/{repo}.git/info/refs",
                &["GET"],
                "git_upload_pack",
            )])
            .await;
        let m = table
            .match_request("GET", "/acme/widgets.git/info/refs")
            .await
            .unwrap();
        assert_eq!(m.params.get("owner").unwrap(), "acme");
        assert_eq!(m.params.get("repo").unwrap(), "widgets");
    }

    #[tokio::test]
    async fn templated_segment_with_suffix_rejects_missing_suffix() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/{owner}/{repo}.git/info/refs",
                &["GET"],
                "git_upload_pack",
            )])
            .await;
        // Path is missing the `.git` suffix → no match.
        assert!(
            table
                .match_request("GET", "/acme/widgets/info/refs")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn multiple_placeholders_in_segment_rejected() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route("he-1", "/{a}{b}", &["GET"], "bad")])
            .await;
        // Multi-placeholder segments are intentionally unsupported —
        // the extraction is ambiguous.
        assert!(table.match_request("GET", "/foo").await.is_none());
    }
}
