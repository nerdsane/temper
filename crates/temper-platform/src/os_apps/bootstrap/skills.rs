//! Skill bootstrap as TemperFS files at path-based scope locations (ADR-002).

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use super::fs::{
    APP_DOCS_ROOT_DIR_ID, APP_DOCS_WORKSPACE_ID, DirectoryBootstrapTarget,
    MarkdownFileBootstrapTarget, ensure_app_docs_workspace, ensure_directory, ensure_markdown_file,
    slug_fragment,
};
use crate::os_apps::AppSkillDefinition;
use crate::state::PlatformState;

/// Bootstrap skills as TemperFS files at path-based scope locations (ADR-002).
///
/// Skills are written to:
/// - `/system/skills/{slug}/SKILL.md` — system-level skills (agent_name = None)
/// - `/agents/{agent-uuid}/skills/{slug}/SKILL.md` — agent-scoped skills
///
/// No Skill entities are created — the file IS the skill.
/// Returns the names of successfully bootstrapped skills.
pub(in crate::os_apps) async fn bootstrap_skills(
    state: &PlatformState,
    tenant_id: &TenantId,
    tenant: &str,
    skills: &[AppSkillDefinition],
    agent_uuid_map: &BTreeMap<String, String>,
) -> Vec<String> {
    if skills.is_empty() {
        return Vec::new();
    }

    // Require TemperFS types (File, Directory).
    let has_fs = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        registry.get_spec(tenant_id, "File").is_some()
            && registry.get_spec(tenant_id, "Directory").is_some()
    };
    if !has_fs {
        tracing::info!(
            tenant,
            count = skills.len(),
            "Skipping skill bootstrap — TemperFS not registered (install temper-fs first)"
        );
        return Vec::new();
    }

    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");

    // Ensure workspace exists.
    if let Err(e) = ensure_app_docs_workspace(state, tenant_id, &agent_ctx).await {
        tracing::warn!(tenant, error = %e, "Failed to ensure app docs workspace for skill bootstrap");
        return Vec::new();
    }

    // Ensure /system/ root directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-system-root",
            name: "system",
            path: "/system",
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /system/ directory");
        return Vec::new();
    }

    // Ensure /system/skills/ directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-system-skills-root",
            name: "skills",
            path: "/system/skills",
            parent_id: Some("os-system-root"),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /system/skills/ directory");
        return Vec::new();
    }

    // Ensure /agents/ root directory exists.
    if let Err(e) = ensure_directory(
        state,
        tenant_id,
        &agent_ctx,
        DirectoryBootstrapTarget {
            directory_id: "os-agents-root",
            name: "agents",
            path: "/agents",
            parent_id: Some(APP_DOCS_ROOT_DIR_ID),
            workspace_id: APP_DOCS_WORKSPACE_ID,
        },
    )
    .await
    {
        tracing::warn!(tenant, error = %e, "Failed to create /agents/ directory");
        return Vec::new();
    }

    let mut bootstrapped = Vec::new();

    for skill in skills {
        let slug = skill.name.to_lowercase().replace(' ', "-");

        // Determine TemperFS path based on scope.
        let (dir_id, file_id, dir_path, file_path, parent_dir_id) = match &skill.agent_name {
            None => {
                // System skill: /system/skills/{slug}/SKILL.md
                let dir_id = format!("os-sys-skill-dir-{slug}");
                let file_id = format!("os-sys-skill-file-{slug}");
                let dir_path = format!("/system/skills/{slug}");
                let file_path = format!("/system/skills/{slug}/SKILL.md");
                (
                    dir_id,
                    file_id,
                    dir_path,
                    file_path,
                    "os-system-skills-root".to_string(),
                )
            }
            Some(agent_name) => {
                // Agent-scoped skill: /agents/{agent-uuid}/skills/{slug}/SKILL.md
                let agent_uuid = match agent_uuid_map.get(agent_name) {
                    Some(uuid) => uuid.clone(),
                    None => {
                        tracing::warn!(
                            tenant,
                            skill = %skill.name,
                            agent = %agent_name,
                            "Agent UUID not found for agent-scoped skill — skipping"
                        );
                        continue;
                    }
                };

                // Ensure /agents/{agent-uuid}/ directory exists.
                let agent_dir_id = format!("os-agent-dir-{}", slug_fragment(&agent_uuid));
                let agent_dir_path = format!("/agents/{agent_uuid}");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &agent_dir_id,
                        name: agent_name,
                        path: &agent_dir_path,
                        parent_id: Some("os-agents-root"),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, agent = %agent_name, error = %e, "Failed to create agent directory");
                    continue;
                }

                // Ensure /agents/{agent-uuid}/skills/ directory exists.
                let agent_skills_dir_id =
                    format!("os-agent-skills-dir-{}", slug_fragment(&agent_uuid));
                let agent_skills_dir_path = format!("/agents/{agent_uuid}/skills");
                if let Err(e) = ensure_directory(
                    state,
                    tenant_id,
                    &agent_ctx,
                    DirectoryBootstrapTarget {
                        directory_id: &agent_skills_dir_id,
                        name: "skills",
                        path: &agent_skills_dir_path,
                        parent_id: Some(&agent_dir_id),
                        workspace_id: APP_DOCS_WORKSPACE_ID,
                    },
                )
                .await
                {
                    tracing::warn!(tenant, agent = %agent_name, error = %e, "Failed to create agent skills directory");
                    continue;
                }

                let dir_id = format!("os-agent-skill-dir-{}-{slug}", slug_fragment(&agent_uuid));
                let file_id = format!("os-agent-skill-file-{}-{slug}", slug_fragment(&agent_uuid));
                let dir_path = format!("/agents/{agent_uuid}/skills/{slug}");
                let file_path = format!("/agents/{agent_uuid}/skills/{slug}/SKILL.md");
                (dir_id, file_id, dir_path, file_path, agent_skills_dir_id)
            }
        };

        // Create skill directory.
        if let Err(e) = ensure_directory(
            state,
            tenant_id,
            &agent_ctx,
            DirectoryBootstrapTarget {
                directory_id: &dir_id,
                name: &slug,
                path: &dir_path,
                parent_id: Some(&parent_dir_id),
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
        )
        .await
        {
            tracing::warn!(tenant, skill = %skill.name, error = %e, "Failed to create skill directory");
            continue;
        }

        // Create SKILL.md and upload content.
        if let Err(e) = ensure_markdown_file(
            state,
            tenant_id,
            &agent_ctx,
            MarkdownFileBootstrapTarget {
                file_id: &file_id,
                name: "SKILL.md",
                path: &file_path,
                directory_id: &dir_id,
                workspace_id: APP_DOCS_WORKSPACE_ID,
            },
            skill.content.as_bytes(),
        )
        .await
        {
            tracing::warn!(tenant, skill = %skill.name, error = %e, "Failed to create SKILL.md file");
            continue;
        }

        // Bootstrap companion files in the same directory.
        for companion in &skill.companion_files {
            let comp_slug = companion
                .name
                .replace(std::path::MAIN_SEPARATOR, "-")
                .to_lowercase();
            let comp_file_id = format!("{file_id}-{comp_slug}");
            let comp_file_path = format!("{dir_path}/{}", companion.name);
            let comp_file_name = companion.name.rsplit('/').next().unwrap_or(&companion.name);
            if let Err(e) = ensure_markdown_file(
                state,
                tenant_id,
                &agent_ctx,
                MarkdownFileBootstrapTarget {
                    file_id: &comp_file_id,
                    name: comp_file_name,
                    path: &comp_file_path,
                    directory_id: &dir_id,
                    workspace_id: APP_DOCS_WORKSPACE_ID,
                },
                &companion.content,
            )
            .await
            {
                tracing::warn!(
                    tenant,
                    skill = %skill.name,
                    companion = %companion.name,
                    error = %e,
                    "Failed to create companion file"
                );
            }
        }

        let scope_label = match &skill.agent_name {
            None => "system",
            Some(a) => a.as_str(),
        };
        tracing::info!(tenant, skill = %skill.name, scope = %scope_label, path = %file_path, "Skill bootstrapped as TemperFS file");
        bootstrapped.push(skill.name.clone());
    }
    bootstrapped
}
