use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use serde_json::json;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

use super::super::types::LoadInlineRequest;
use crate::state::ServerState;

pub(super) async fn build_adr_warning_context(
    state: &ServerState,
    body: &LoadInlineRequest,
    tenant: &str,
) -> Option<serde_json::Value> {
    let namespaces = extract_submitted_namespaces(&body.specs);
    let candidate_paths = adr_candidate_paths(body.app_name.as_deref(), &namespaces);
    if candidate_paths.is_empty() {
        return None;
    }

    let hits = find_existing_adr_paths(state, tenant, &candidate_paths).await;
    if !hits.is_empty() {
        return None;
    }

    tracing::warn!(
        tenant,
        app_name = body.app_name.as_deref().unwrap_or(""),
        namespaces = ?namespaces,
        candidate_paths = ?candidate_paths,
        "Spec submitted with no ADRs — design decisions should be recorded under /apps/<app>/adrs/"
    );

    Some(json!({
        "warnings": [{
            "code": "missing_adrs",
            "message": "Spec submitted with no ADRs — design decisions should be recorded under /apps/<app>/adrs/.",
            "candidate_paths": candidate_paths,
            "namespaces": namespaces,
            "app_name": body.app_name,
        }]
    }))
}

fn extract_submitted_namespaces(specs: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut namespaces = std::collections::BTreeSet::new();
    for (filename, content) in specs {
        if !filename.ends_with(".csdl.xml") {
            continue;
        }
        if let Ok(document) = parse_csdl(content) {
            for schema in document.schemas {
                if schema.namespace.ends_with(".Vocab") || schema.namespace == "Temper.Vocab" {
                    continue;
                }
                namespaces.insert(schema.namespace);
            }
        }
    }
    namespaces.into_iter().collect()
}

const INLINE_SPEC_PATH_BUDGET: usize = 512;
const INLINE_SPEC_COMPONENT_BUDGET: usize = 255;

fn validate_inline_spec_path(path: &str) -> Result<(), (StatusCode, String)> {
    use std::path::Component;

    if path.is_empty() || path.len() > INLINE_SPEC_PATH_BUDGET {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Invalid inline spec path length: {path:?}"),
        ));
    }
    let mut components = 0usize;
    for component in Path::new(path).components() {
        let Component::Normal(component) = component else {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Inline spec paths must be relative and normalized: {path:?}"),
            ));
        };
        if component.is_empty() || component.as_encoded_bytes().len() > INLINE_SPEC_COMPONENT_BUDGET
        {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("Inline spec path component exceeds its budget: {path:?}"),
            ));
        }
        components += 1;
    }
    if components == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Inline spec path has no filename: {path:?}"),
        ));
    }
    Ok(())
}

pub(super) fn resolve_inline_specs_root(
    tmp_dir: &Path,
    specs: &std::collections::BTreeMap<String, String>,
) -> Result<PathBuf, (StatusCode, String)> {
    for path in specs.keys() {
        validate_inline_spec_path(path)?;
    }

    let model_paths: Vec<&str> = specs
        .keys()
        .filter_map(|path| {
            (Path::new(path).file_name().and_then(|name| name.to_str()) == Some("model.csdl.xml"))
                .then_some(path.as_str())
        })
        .collect();

    if model_paths.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Inline spec submission must include model.csdl.xml".to_string(),
        ));
    }

    if model_paths.len() > 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Inline spec submission must contain exactly one model.csdl.xml, found {}",
                model_paths.len()
            ),
        ));
    }

    let model_path = Path::new(model_paths[0]);
    let relative_root = model_path.parent().unwrap_or_else(|| Path::new(""));
    for path in specs.keys().map(Path::new) {
        if !path.starts_with(relative_root) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "Inline spec path {path:?} is outside the model.csdl.xml directory {relative_root:?}"
                ),
            ));
        }
    }
    Ok(if relative_root.as_os_str().is_empty() {
        tmp_dir.to_path_buf()
    } else {
        tmp_dir.join(relative_root)
    })
}

fn adr_candidate_paths(app_name: Option<&str>, namespaces: &[String]) -> Vec<String> {
    let mut candidates = std::collections::BTreeSet::new();
    if let Some(app_name) = app_name {
        let normalized = normalize_app_slug(app_name);
        if !normalized.is_empty() {
            candidates.insert(format!("/apps/{normalized}/adrs/"));
        }
    }

    for namespace in namespaces {
        for candidate in namespace_to_app_candidates(namespace) {
            if !candidate.is_empty() {
                candidates.insert(format!("/apps/{candidate}/adrs/"));
            }
        }
    }

    candidates.into_iter().collect()
}

fn namespace_to_app_candidates(namespace: &str) -> Vec<String> {
    let parts: Vec<&str> = namespace
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }

    let full = normalize_app_slug(namespace);
    let remainder = if parts.len() > 1 {
        kebab_join(&parts[1..])
    } else {
        normalize_app_slug(parts[0])
    };

    let mut candidates = std::collections::BTreeSet::new();
    if !full.is_empty() {
        candidates.insert(full);
    }
    if !remainder.is_empty() {
        candidates.insert(remainder.clone());
    }
    match parts[0].to_ascii_lowercase().as_str() {
        "openpaw" | "paw" => {
            if !remainder.is_empty() {
                candidates.insert(format!("paw-{remainder}"));
            }
        }
        "temper" => {
            if !remainder.is_empty() {
                candidates.insert(format!("temper-{remainder}"));
            }
        }
        _ => {}
    }
    candidates.into_iter().collect()
}

fn kebab_join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| normalize_app_slug(part))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn normalize_app_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut prev_was_sep = true;
    let mut prev_was_lower_or_digit = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            let is_upper = ch.is_ascii_uppercase();
            if is_upper && prev_was_lower_or_digit && !prev_was_sep {
                slug.push('-');
            }
            slug.push(ch.to_ascii_lowercase());
            prev_was_sep = false;
            prev_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else if !prev_was_sep {
            slug.push('-');
            prev_was_sep = true;
            prev_was_lower_or_digit = false;
        }
    }

    slug.trim_matches('-').to_string()
}

async fn find_existing_adr_paths(
    state: &ServerState,
    tenant: &str,
    candidate_paths: &[String],
) -> Vec<String> {
    let tenant_id = TenantId::new(tenant);
    let has_files = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(&tenant_id, "File").is_some()
    };
    if !has_files {
        return Vec::new();
    }

    let mut hits = Vec::new();
    for file_id in state.list_entity_ids(&tenant_id, "File") {
        let Ok(resp) = state
            .get_tenant_entity_state(&tenant_id, "File", &file_id)
            .await
        else {
            continue;
        };
        if resp.state.status == "Archived" {
            continue;
        }
        let path = resp
            .state
            .fields
            .get("Path")
            .and_then(|value| value.as_str())
            .or_else(|| {
                resp.state
                    .fields
                    .get("path")
                    .and_then(|value| value.as_str())
            });
        let Some(path) = path else { continue };
        if candidate_paths
            .iter()
            .any(|prefix| path.starts_with(prefix))
        {
            hits.push(path.to_string());
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        adr_candidate_paths, namespace_to_app_candidates, normalize_app_slug,
        resolve_inline_specs_root,
    };

    #[test]
    fn normalize_app_slug_kebab_cases_namespaces() {
        assert_eq!(
            normalize_app_slug("Temper.ProjectManagement"),
            "temper-project-management"
        );
        assert_eq!(normalize_app_slug("OpenPaw"), "open-paw");
        assert_eq!(normalize_app_slug("llm-wiki"), "llm-wiki");
    }

    #[test]
    fn namespace_to_app_candidates_adds_platform_aware_variants() {
        let paw = namespace_to_app_candidates("OpenPaw.Foresight");
        assert!(paw.contains(&"open-paw-foresight".to_string()));
        assert!(paw.contains(&"foresight".to_string()));
        assert!(paw.contains(&"paw-foresight".to_string()));

        let temper = namespace_to_app_candidates("Temper.ProjectManagement");
        assert!(temper.contains(&"temper-project-management".to_string()));
        assert!(temper.contains(&"project-management".to_string()));
    }

    #[test]
    fn adr_candidate_paths_prefers_explicit_app_name() {
        let paths =
            adr_candidate_paths(Some("llm-wiki"), &["Temper.ProjectManagement".to_string()]);
        assert!(paths.contains(&"/apps/llm-wiki/adrs/".to_string()));
        assert!(paths.contains(&"/apps/project-management/adrs/".to_string()));
    }

    #[test]
    fn inline_spec_paths_cannot_escape_isolated_staging() {
        for malicious in [
            "../model.csdl.xml",
            "/tmp/model.csdl.xml",
            "nested/../../model.csdl.xml",
            "./model.csdl.xml",
        ] {
            let specs = BTreeMap::from([(malicious.to_string(), String::new())]);
            let error = resolve_inline_specs_root(std::path::Path::new("/tmp/staging"), &specs)
                .expect_err("escaping path should be rejected");
            assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn inline_spec_paths_must_share_the_model_directory() {
        let specs = BTreeMap::from([
            ("app/model.csdl.xml".to_string(), String::new()),
            ("other/order.ioa.toml".to_string(), String::new()),
        ]);
        let error = resolve_inline_specs_root(std::path::Path::new("/tmp/staging"), &specs)
            .expect_err("out-of-root file should be rejected");
        assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn inline_spec_model_name_is_exact_not_a_suffix() {
        let specs = BTreeMap::from([("not-model.csdl.xml".to_string(), String::new())]);
        let error = resolve_inline_specs_root(std::path::Path::new("/tmp/staging"), &specs)
            .expect_err("model filename suffix should not be accepted");
        assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn inline_spec_paths_allow_one_normalized_nested_root() {
        let specs = BTreeMap::from([
            ("app/model.csdl.xml".to_string(), String::new()),
            ("app/order.ioa.toml".to_string(), String::new()),
        ]);
        let root = resolve_inline_specs_root(std::path::Path::new("/tmp/staging"), &specs)
            .expect("normalized paths should be accepted");
        assert_eq!(root, std::path::Path::new("/tmp/staging/app"));
    }
}
