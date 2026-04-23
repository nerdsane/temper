//! In-memory routing table for ADR-0056 `HttpEndpoint` entities.
//!
//! The kernel's axum router consults this table as a fallback (after
//! built-in routes). On each request, the table does longest-prefix
//! match on the request path against all `Active` HttpEndpoint rows
//! registered for the tenant, extracts templated path parameters,
//! and returns the matched route. The dispatcher then opens an
//! inbound exchange (ADR-0057 Phase 2) and invokes the bound WASM
//! integration.
//!
//! Discipline (per ADR-0056):
//!   * Longest-prefix match wins; ties on length are rejected at
//!     `Create` time by a cross-field invariant.
//!   * Built-in namespaces (`/tdata`, `/webhooks`, `/_admin`,
//!     `/observe`, `/api`, `/_internal`) are reserved; the spec's
//!     invariants block them at write time.
//!   * Methods are matched case-insensitively against the row's
//!     comma-separated Methods column.
//!   * `Paused` / `Deleted` endpoints never match.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use tokio::sync::RwLock;

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

/// Project a raw `fields` JSON object from an `HttpEndpoint`
/// entity row into a typed [`HttpEndpointRoute`]. Returns `None`
/// if required fields are missing or malformed — caller treats
/// that as "skip this row" in the reconciler path.
pub fn route_from_entity_fields(
    id: &str,
    fields: &serde_json::Value,
) -> Option<HttpEndpointRoute> {
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
    Some(HttpEndpointRoute {
        id: id.to_string(),
        path_prefix,
        methods,
        integration_module,
        requires_auth,
        timeout_secs: timeout_secs.min(u32::MAX as u64) as u32,
    })
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
            (Some(t), Some(p)) => {
                if let Some(name) = t.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                    if p.is_empty() {
                        return None;
                    }
                    params.insert(name.to_string(), p.to_string());
                } else if t != p {
                    return None;
                }
            }
            (Some(_), None) => return None,
        }
    }
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
            .replace(vec![route(
                "he-1",
                "/hello",
                &["GET"],
                "hello_handler",
            )])
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
            .replace(vec![route(
                "he-1",
                "/refs",
                &["GET"],
                "refs_read",
            )])
            .await;
        assert!(table.match_request("POST", "/refs").await.is_none());
    }

    #[tokio::test]
    async fn method_match_case_insensitive() {
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/refs",
                &["POST"],
                "refs_write",
            )])
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
            .replace(vec![route(
                "he-1",
                "/{owner}/repos",
                &["GET"],
                "h",
            )])
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
        });
        let r = route_from_entity_fields("he-abc", &fields).unwrap();
        assert_eq!(r.id, "he-abc");
        assert_eq!(r.methods, vec!["GET".to_string()]);
        assert_eq!(r.integration_module, "git_upload_pack");
        assert!(r.requires_auth);
        assert_eq!(r.timeout_secs, 120);
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
    async fn mixed_templated_segment_not_supported() {
        // Per ADR-0056, only whole-segment templates are supported.
        // `{repo}.git` is NOT a template segment — it's a literal
        // that starts with `{`. The table must reject the path that
        // only differs by value.
        let table = HttpEndpointTable::new();
        table
            .replace(vec![route(
                "he-1",
                "/repos/{owner}/{repo}.git",
                &["GET"],
                "bad",
            )])
            .await;
        assert!(
            table
                .match_request("GET", "/repos/acme/widgets.git")
                .await
                .is_none()
        );
    }
}
