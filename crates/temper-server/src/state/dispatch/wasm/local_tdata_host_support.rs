use std::collections::{BTreeMap, BTreeSet};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Url;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::tenant::TenantId;

pub(super) fn callback_string<'a>(
    body: &'a serde_json::Value,
    snake_name: &str,
    pascal_name: &str,
) -> Result<&'a str, String> {
    body.get(snake_name)
        .or_else(|| body.get(pascal_name))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("callback registration is missing {snake_name:?}"))
}

pub(super) fn pending_decision_filter_id(filter: &str) -> Option<String> {
    let encoded = filter
        .strip_prefix("pending_decision_id eq '")?
        .strip_suffix('\'')?;
    if encoded.is_empty() {
        return None;
    }
    Some(encoded.replace("''", "'"))
}

pub(super) fn governance_callback_decision_id(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let mut segments = parsed.path_segments()?.rev();
    let action = segments.next()?.rsplit('.').next()?;
    if action != "RegisterCallback" {
        return None;
    }
    let entity = segments.next()?;
    let key = entity
        .strip_prefix("GovernanceDecisions(")?
        .strip_suffix(')')?;
    let key = key.strip_prefix('\'')?.strip_suffix('\'')?;
    (!key.is_empty()).then(|| key.replace("''", "'"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalTDataRequest {
    pub(super) path: String,
    pub(super) query: BTreeMap<String, String>,
}

impl LocalTDataRequest {
    pub(super) fn parse(url: &str, local_tdata_hosts: &BTreeSet<String>) -> Option<Self> {
        let parsed = Url::parse(url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?;
        if !is_local_tdata_host(host, local_tdata_hosts) {
            return None;
        }

        let raw_path = parsed.path();
        let path = match raw_path {
            "/tdata" | "/tdata/" => String::new(),
            _ => raw_path.strip_prefix("/tdata/")?.to_string(),
        };
        if is_file_value_path(&path) {
            return None;
        }

        let query = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<BTreeMap<_, _>>();

        Some(Self { path, query })
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

fn is_local_tdata_host(host: &str, local_tdata_hosts: &BTreeSet<String>) -> bool {
    is_loopback_host(host) || local_tdata_hosts.contains(&host.to_ascii_lowercase())
}

fn is_file_value_path(path: &str) -> bool {
    path.starts_with("Files('") && path.ends_with("')/$value")
}

pub(super) fn header_map(
    headers: &[(String, String)],
    tenant: &TenantId,
    inherited_headers: &[(String, String)],
) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if !is_temper_trust_header(name) {
            insert_header(&mut map, name, value);
        }
    }
    for (name, value) in inherited_headers {
        insert_header(&mut map, name, value);
    }
    let value =
        HeaderValue::from_str(tenant.as_str()).expect("TenantId is a valid HTTP header value");
    map.insert(HeaderName::from_static("x-tenant-id"), value);
    map
}

pub(super) fn callback_registration_header_map(headers: &[(String, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if !is_temper_trust_header(name) {
            insert_header(&mut map, name, value);
        }
    }
    map.insert(
        HeaderName::from_static("x-tenant-id"),
        HeaderValue::from_static("temper-system"),
    );
    map.insert(
        HeaderName::from_static("x-temper-principal-kind"),
        HeaderValue::from_static("admin"),
    );
    map
}

pub(super) fn strip_untrusted_temper_headers(
    headers: &[(String, String)],
) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| !is_temper_trust_header(name))
        .cloned()
        .collect()
}

pub(super) fn is_temper_trust_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name == "x-tenant-id"
        || name == "x-temper-tenant-id"
        || name.starts_with("x-temper-principal-")
        || name.starts_with("x-temper-agent-")
        || name == "x-temper-action-context"
}

fn insert_header(map: &mut HeaderMap, name: &str, value: &str) {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
        return;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
        return;
    };
    map.insert(name, value);
}

pub(super) fn security_context_headers(ctx: &SecurityContext) -> Vec<(String, String)> {
    let mut headers = vec![(
        "x-temper-principal-id".to_string(),
        ctx.principal.id.clone(),
    )];
    let kind = match ctx.principal.kind {
        PrincipalKind::Customer => "customer",
        PrincipalKind::Agent => "agent",
        PrincipalKind::Admin => "admin",
        // External parsing rejects System. A trusted in-process invocation is
        // attenuated to the narrower Agent principal understood by app policy;
        // guest-provided identity headers are stripped before these host-owned
        // headers are applied.
        PrincipalKind::System => "agent",
    };
    headers.push(("x-temper-principal-kind".to_string(), kind.to_string()));
    if let Some(role) = &ctx.principal.role {
        headers.push(("x-temper-agent-role".to_string(), role.clone()));
    }
    if let Some(agent_type) = &ctx.principal.agent_type {
        headers.push(("x-temper-agent-type".to_string(), agent_type.clone()));
    } else if matches!(ctx.principal.kind, PrincipalKind::System) {
        headers.push(("x-temper-agent-type".to_string(), "system".to_string()));
    }
    if let Some(scopes) = ctx
        .principal
        .attributes
        .get("scopes")
        .and_then(|value| value.as_array())
    {
        let scopes = scopes
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>()
            .join(",");
        if !scopes.is_empty() {
            headers.push(("x-temper-principal-scopes".to_string(), scopes));
        }
    }
    if let Some(action_context) = ctx
        .principal
        .attributes
        .get("action_context")
        .and_then(|value| value.as_str())
    {
        headers.push((
            "x-temper-action-context".to_string(),
            action_context.to_string(),
        ));
    }
    headers
}
