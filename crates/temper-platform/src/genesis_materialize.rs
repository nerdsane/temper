use std::collections::BTreeSet;
use std::path::Path;

use base64::Engine as _;
use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::state::{DispatchCommand, ServerState};

use crate::os_apps::AppManifest;

#[derive(Debug, Clone)]
pub(crate) struct GenesisAppBundle {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) repository_id: String,
    pub(crate) version_hash: String,
}

pub(crate) async fn materialize_app_closure(
    state: &ServerState,
    tenant: &TenantId,
    cache_root: &Path,
    root: GenesisAppBundle,
) -> Result<Vec<String>, String> {
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();
    let mut materialized = Vec::new();

    while let Some(app) = stack.pop() {
        if !seen.insert(app.name.clone()) {
            continue;
        }

        let app_dir = cache_root.join(&app.name);
        materialize_commit_tree(
            state,
            tenant,
            &app.repository_id,
            &app.version_hash,
            &app_dir,
        )
        .await?;
        materialized.push(app.name.clone());

        for dependency in read_manifest_dependencies(&app_dir)?.into_iter().rev() {
            let dependency = resolve_genesis_dependency(state, tenant, &app.owner, &dependency)
                .await
                .map_err(|error| {
                    format!(
                        "resolve dependency '{}' for Genesis app '{}': {error}",
                        dependency, app.name
                    )
                })?;
            if !seen.contains(&dependency.name) {
                stack.push(dependency);
            }
        }
    }

    Ok(materialized)
}

fn read_manifest_dependencies(app_dir: &Path) -> Result<Vec<String>, String> {
    let path = app_dir.join("app.toml");
    let content = std::fs::read_to_string(&path)
        .map_err(|error| format!("read Genesis app manifest '{}': {error}", path.display()))?;
    let manifest: AppManifest = toml::from_str(&content)
        .map_err(|error| format!("parse Genesis app manifest '{}': {error}", path.display()))?;
    Ok(manifest.dependencies)
}

async fn resolve_genesis_dependency(
    state: &ServerState,
    tenant: &TenantId,
    preferred_owner: &str,
    dependency: &str,
) -> Result<GenesisAppBundle, String> {
    let requested = parse_dependency_ref(dependency, preferred_owner);
    let ids = state.list_entity_ids_lazy(tenant, "App").await;
    let mut matches = Vec::new();

    for entity_id in ids {
        let candidate = state
            .get_tenant_entity_state(tenant, "App", &entity_id)
            .await
            .map_err(|error| format!("read Genesis App {entity_id}: {error}"))?;
        if candidate.state.status != "Active" {
            continue;
        }
        let fields = &candidate.state.fields;
        let Some(name) = string_field(fields, "Name") else {
            continue;
        };
        if name != requested.name {
            continue;
        }
        let Some(owner) = string_field(fields, "OwnerId") else {
            continue;
        };
        if let Some(requested_owner) = requested.owner.as_deref()
            && owner != requested_owner
        {
            continue;
        }
        let Some(repository_id) = string_field(fields, "RepositoryId") else {
            continue;
        };
        let version_hash = requested
            .version_hash
            .clone()
            .or_else(|| string_field(fields, "LatestVersionHash"))
            .ok_or_else(|| format!("Genesis App {entity_id} is missing LatestVersionHash"))?;
        matches.push(GenesisAppBundle {
            owner,
            name,
            repository_id,
            version_hash,
        });
    }

    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    if matches.is_empty() {
        return Err(format!(
            "no active Genesis App row found for '{}'",
            dependency
        ));
    }

    matches
        .into_iter()
        .find(|app| app.owner == preferred_owner)
        .ok_or_else(|| format!("multiple Genesis App rows match '{}'", dependency))
}

#[derive(Debug, PartialEq, Eq)]
struct DependencyRef {
    owner: Option<String>,
    name: String,
    version_hash: Option<String>,
}

fn parse_dependency_ref(input: &str, preferred_owner: &str) -> DependencyRef {
    let trimmed = input.trim();
    let (owner_and_name, version_hash) = trimmed
        .split_once('@')
        .map(|(left, right)| (left, Some(right.trim_start_matches('@').to_string())))
        .unwrap_or((trimmed, None));
    let (owner, name) = owner_and_name
        .split_once('/')
        .map(|(owner, name)| (Some(owner.to_string()), name.to_string()))
        .unwrap_or_else(|| {
            let owner = if preferred_owner.is_empty() {
                None
            } else {
                Some(preferred_owner.to_string())
            };
            (owner, owner_and_name.to_string())
        });

    DependencyRef {
        owner,
        name,
        version_hash,
    }
}

pub(crate) async fn mark_installation(
    state: &ServerState,
    tenant: &TenantId,
    installation_id: &str,
    action: &str,
    params: Value,
) {
    let agent_ctx = temper_server::request_context::AgentContext::system();
    let _ = state
        .dispatch(DispatchCommand {
            tenant,
            entity_type: "AppInstallation",
            entity_id: installation_id,
            action,
            params,
            agent_ctx: &agent_ctx,
            await_integration: false,
        })
        .await;
}

async fn materialize_commit_tree(
    state: &ServerState,
    tenant: &TenantId,
    repository_id: &str,
    version_hash: &str,
    app_dir: &Path,
) -> Result<(), String> {
    let commit_id = version_hash.trim_start_matches('@');
    let commit = load_genesis_object(state, tenant, "Commit", repository_id, commit_id)
        .await?
        .ok_or_else(|| format!("Genesis commit {commit_id} not found for {repository_id}"))?;
    let tree_sha = string_field(&commit.state.fields, "TreeSha")
        .ok_or_else(|| format!("Genesis commit {commit_id} is missing TreeSha"))?;

    if app_dir.exists() {
        std::fs::remove_dir_all(app_dir)
            .map_err(|e| format!("clear Genesis app cache '{}': {e}", app_dir.display()))?;
    }
    std::fs::create_dir_all(app_dir)
        .map_err(|e| format!("create Genesis app cache '{}': {e}", app_dir.display()))?;
    materialize_tree(state, tenant, repository_id, &tree_sha, app_dir).await
}

async fn materialize_tree(
    state: &ServerState,
    tenant: &TenantId,
    repository_id: &str,
    tree_sha: &str,
    dir: &Path,
) -> Result<(), String> {
    let mut stack = vec![(tree_sha.to_string(), dir.to_path_buf())];
    while let Some((current_tree, current_dir)) = stack.pop() {
        std::fs::create_dir_all(&current_dir)
            .map_err(|e| format!("create directory '{}': {e}", current_dir.display()))?;
        let tree = load_genesis_object(state, tenant, "Tree", repository_id, &current_tree)
            .await?
            .ok_or_else(|| format!("Genesis tree {current_tree} not found for {repository_id}"))?;
        let canonical = string_field(&tree.state.fields, "CanonicalBytes")
            .ok_or_else(|| format!("Genesis tree {current_tree} is missing CanonicalBytes"))?;
        for entry in parse_tree_entries(&decode_git_object_body(&canonical, "tree")?)? {
            validate_tree_entry_name(&entry.name)?;
            let path = current_dir.join(&entry.name);
            if entry.is_tree() {
                stack.push((entry.object_sha, path));
                continue;
            }
            let blob = load_genesis_object(state, tenant, "Blob", repository_id, &entry.object_sha)
                .await?
                .ok_or_else(|| {
                    format!(
                        "Genesis blob {} not found for {}",
                        entry.object_sha, repository_id
                    )
                })?;
            let blob_repository = string_field(&blob.state.fields, "RepositoryId")
                .unwrap_or_else(|| repository_id.to_string());
            if blob_repository != repository_id {
                return Err(format!(
                    "blob {} belongs to repository {}, expected {}",
                    entry.object_sha, blob_repository, repository_id
                ));
            }
            let content = string_field(&blob.state.fields, "Content")
                .ok_or_else(|| format!("Genesis blob {} is missing Content", entry.object_sha))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create directory '{}': {e}", parent.display()))?;
            }
            std::fs::write(&path, decode_blob_content(&content))
                .map_err(|e| format!("write Genesis app file '{}': {e}", path.display()))?;
        }
    }
    Ok(())
}

async fn load_genesis_object(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    repository_id: &str,
    git_sha: &str,
) -> Result<Option<temper_server::EntityResponse>, String> {
    if state.entity_exists(tenant, entity_type, git_sha)
        && let Ok(found) = state
            .get_tenant_entity_state(tenant, entity_type, git_sha)
            .await
    {
        let fields = &found.state.fields;
        let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();
        let object_sha = string_field(fields, "Id").unwrap_or_default();
        if object_repo == repository_id && object_sha == git_sha {
            return Ok(Some(found));
        }
    }

    let ids = state.list_entity_ids_lazy(tenant, entity_type).await;
    for entity_id in ids {
        let candidate = state
            .get_tenant_entity_state(tenant, entity_type, &entity_id)
            .await
            .map_err(|e| format!("read Genesis {entity_type} {entity_id}: {e}"))?;
        let fields = &candidate.state.fields;
        let object_repo = string_field(fields, "RepositoryId").unwrap_or_default();
        let object_sha = string_field(fields, "Id").unwrap_or_default();
        if object_repo == repository_id && object_sha == git_sha {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

#[derive(Debug)]
struct TreeEntry {
    mode: String,
    name: String,
    object_sha: String,
}

impl TreeEntry {
    fn is_tree(&self) -> bool {
        self.mode == "40000" || self.mode == "040000"
    }
}

fn parse_tree_entries(body: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        let mode_start = offset;
        while offset < body.len() && body[offset] != b' ' {
            offset += 1;
        }
        if offset >= body.len() {
            return Err("malformed tree entry mode".to_string());
        }
        let mode = std::str::from_utf8(&body[mode_start..offset])
            .map_err(|e| format!("tree mode is not UTF-8: {e}"))?
            .to_string();
        offset += 1;

        let name_start = offset;
        while offset < body.len() && body[offset] != 0 {
            offset += 1;
        }
        if offset >= body.len() {
            return Err("malformed tree entry name".to_string());
        }
        let name = std::str::from_utf8(&body[name_start..offset])
            .map_err(|e| format!("tree path is not UTF-8: {e}"))?
            .to_string();
        offset += 1;

        if offset + 20 > body.len() {
            return Err("malformed tree entry object id".to_string());
        }
        let object_sha = body[offset..offset + 20]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        offset += 20;
        entries.push(TreeEntry {
            mode,
            name,
            object_sha,
        });
    }
    Ok(entries)
}

fn validate_tree_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(format!("unsafe Genesis tree entry path '{name}'"));
    }
    Ok(())
}

fn decode_blob_content(value: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .unwrap_or_else(|_| value.as_bytes().to_vec())
}

fn decode_git_object_body(value: &str, expected_kind: &str) -> Result<Vec<u8>, String> {
    let canonical = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| format!("CanonicalBytes must be base64: {e}"))?;
    let Some(nul) = canonical.iter().position(|byte| *byte == 0) else {
        return Err("CanonicalBytes missing git object header terminator".to_string());
    };
    let header = std::str::from_utf8(&canonical[..nul])
        .map_err(|e| format!("CanonicalBytes header is not UTF-8: {e}"))?;
    let expected_prefix = format!("{expected_kind} ");
    if !header.starts_with(&expected_prefix) {
        return Err(format!(
            "CanonicalBytes header must start with '{expected_prefix}'"
        ));
    }
    Ok(canonical[nul + 1..].to_vec())
}

pub(crate) fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .or_else(|| value.get("fields").and_then(|fields| fields.get(key)))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}
