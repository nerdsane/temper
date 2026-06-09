//! Install helpers for `temper install`.
//!
//! With no positional app ref this copies Claude/Codex helper skills into
//! `~/.claude/skills`. With an `owner/app@hash` ref it invokes the spec-owned
//! Genesis `App.Install` action on a Temper server.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// The Temper App Builder skill body, embedded at compile time.
const SKILL_BODY: &str = include_str!("../../../../skills/temper-developer/SKILL.md");

/// YAML frontmatter required by Claude Code's skill loader.
const SKILL_FRONTMATTER: &str = "\
---
name: temper-developer
description: \"You MUST use this skill when the user asks to build an app, create an application, make a tool, or says 'build me a X'. Temper builds apps from verified specs, not from code.\"
---
";

/// The Temper User (production chat proxy) skill body, embedded at compile time.
const USER_SKILL_BODY: &str = include_str!("../../../../skills/temper-user/SKILL.md");

/// YAML frontmatter for the temper-user skill.
const USER_SKILL_FRONTMATTER: &str = "\
---
name: temper-user
description: \"Production chat proxy for a running Temper application. Translates natural language into OData API calls. Use when the user wants to interact with a running Temper app as an end user.\"
---
";

/// The Temper Agent skill body (MCP operations), embedded at compile time.
const AGENT_SKILL_BODY: &str = include_str!("../../../../skills/temper-agent/SKILL.md");

/// YAML frontmatter for the temper-agent skill.
const AGENT_SKILL_FRONTMATTER: &str = "\
---
name: temper-agent
description: \"Governed MCP operations. Use when Claude Code needs to operate through mcp__temper__execute or mcp__temper__search. Teaches the Python sandbox API, IOA spec format, and governance flow.\"
---
";
/// Install the skill to the given skills root directory (for testing with custom paths).
fn install_to(skills_root: &Path) -> Result<()> {
    let skill_dir = skills_root.join("temper-developer");
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create directory: {}", skill_dir.display()))?;

    let skill_path = skill_dir.join("SKILL.md");
    let content = format!("{SKILL_FRONTMATTER}{SKILL_BODY}");
    fs::write(&skill_path, &content)
        .with_context(|| format!("Failed to write {}", skill_path.display()))?;

    // Install temper-user skill (production chat proxy)
    let user_skill_dir = skills_root.join("temper-user");
    fs::create_dir_all(&user_skill_dir)
        .with_context(|| format!("Failed to create directory: {}", user_skill_dir.display()))?;

    let user_skill_path = user_skill_dir.join("SKILL.md");
    let user_content = format!("{USER_SKILL_FRONTMATTER}{USER_SKILL_BODY}");
    fs::write(&user_skill_path, &user_content)
        .with_context(|| format!("Failed to write {}", user_skill_path.display()))?;

    // Install temper-agent skill (MCP governed operations)
    let agent_skill_dir = skills_root.join("temper-agent");
    fs::create_dir_all(&agent_skill_dir)
        .with_context(|| format!("Failed to create directory: {}", agent_skill_dir.display()))?;

    let agent_skill_path = agent_skill_dir.join("SKILL.md");
    let agent_content = format!("{AGENT_SKILL_FRONTMATTER}{AGENT_SKILL_BODY}");
    fs::write(&agent_skill_path, &agent_content)
        .with_context(|| format!("Failed to write {}", agent_skill_path.display()))?;

    // Clean up legacy bare files from older installs
    let legacy_path = skills_root.join("temper.md");
    if legacy_path.is_file() {
        let _ = fs::remove_file(&legacy_path);
    }
    let legacy_user_path = skills_root.join("temper-user.md");
    if legacy_user_path.is_file() {
        let _ = fs::remove_file(&legacy_user_path);
    }
    // Clean up old "temper" directory (renamed to "temper-developer")
    let legacy_dir = skills_root.join("temper");
    if legacy_dir.is_dir() {
        let _ = fs::remove_dir_all(&legacy_dir);
    }

    println!("Installed Temper skills to {}", skills_root.display());
    println!("  - {}", skill_path.display());
    println!("  - {}", user_skill_path.display());
    println!("  - {}", agent_skill_path.display());
    println!("\nYou can now open Claude Code in any directory and say:");
    println!("  \"Build me a [your app idea]\"  (uses /temper-developer)");
    println!("  \"/temper-user\"                 (production chat proxy)");
    println!("  \"/temper-agent\"                (MCP governed operations)");

    Ok(())
}

/// Run the `temper install` command.
///
/// Copies the embedded Temper App Builder skill to `~/.claude/skills/temper/SKILL.md`.
pub fn run() -> Result<()> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let skills_root = home.join(".claude").join("skills");
    install_to(&skills_root)
}

/// Install a pinned Genesis app ref into a Temper tenant through OData.
pub async fn run_genesis_app(
    base_url: &str,
    registry_tenant: &str,
    target_tenant: &str,
    app_ref: &str,
    installer: &str,
    follow_policy: &str,
) -> Result<()> {
    let (owner, name, _hash) = parse_app_ref(app_ref)?;
    let follow_policy = normalize_follow_policy(follow_policy)?;
    let app_id = format!(
        "app-{}-{}",
        sanitize_id_component(owner),
        sanitize_id_component(name)
    );
    let url = format!(
        "{}/tdata/Apps('{}')/App.Install",
        base_url.trim_end_matches('/'),
        app_id.replace('\'', "''")
    );
    let response = reqwest::Client::new()
        .post(&url)
        .header("X-Tenant-Id", registry_tenant)
        .json(&serde_json::json!({
            "TargetTenant": target_tenant,
            "AppRef": app_ref,
            "Installer": installer,
            "FollowPolicy": follow_policy,
        }))
        .send()
        .await
        .with_context(|| format!("failed to call Genesis App.Install at {url}"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Genesis app install failed ({status}): {body}");
    }
    println!("Installed Genesis app {app_ref} into tenant {target_tenant}");
    if !body.trim().is_empty() {
        println!("{body}");
    }
    Ok(())
}

fn normalize_follow_policy(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "pinned" => Ok("pinned"),
        "follow_latest" | "follow-latest" => Ok("follow_latest"),
        other => {
            anyhow::bail!("invalid --follow-policy '{other}', expected 'pinned' or 'follow_latest'")
        }
    }
}

fn parse_app_ref(app_ref: &str) -> Result<(&str, &str, &str)> {
    let (path, hash) = app_ref
        .split_once('@')
        .filter(|(_, hash)| !hash.trim().is_empty())
        .with_context(|| format!("invalid Genesis app ref '{app_ref}', expected owner/app@hash"))?;
    let (owner, name) = path
        .split_once('/')
        .filter(|(owner, name)| !owner.trim().is_empty() && !name.trim().is_empty())
        .with_context(|| format!("invalid Genesis app ref '{app_ref}', expected owner/app@hash"))?;
    Ok((owner, name, hash))
}

fn sanitize_id_component(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_content_is_embedded() {
        assert!(
            SKILL_BODY.contains("Temper App Builder"),
            "embedded skill must mention Temper App Builder"
        );
        assert!(
            SKILL_BODY.contains("Interview Protocol"),
            "embedded skill must contain Interview Protocol section"
        );
        assert!(
            SKILL_BODY.contains("IOA TOML Spec Format"),
            "embedded skill must contain IOA TOML Spec Format section"
        );
    }

    #[test]
    fn skill_frontmatter_is_valid() {
        assert!(SKILL_FRONTMATTER.starts_with("---\n"));
        assert!(SKILL_FRONTMATTER.contains("name: temper"));
        assert!(SKILL_FRONTMATTER.contains("description:"));
        assert!(SKILL_FRONTMATTER.ends_with("---\n"));
    }

    #[test]
    fn install_creates_skill_directory_and_file() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_root = tmp.path().join(".claude").join("skills");

        install_to(&skills_root).expect("install_to should succeed");

        let skill_path = skills_root.join("temper-developer").join("SKILL.md");
        assert!(skill_path.is_file(), "SKILL.md should exist after install");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(
            content.starts_with("---\n"),
            "installed file should start with YAML frontmatter"
        );
        assert!(
            content.contains("name: temper-developer"),
            "installed file should have skill name in frontmatter"
        );
        assert!(
            content.contains("Temper App Builder"),
            "installed file should contain skill body"
        );
    }

    #[test]
    fn user_skill_content_is_embedded() {
        assert!(
            USER_SKILL_BODY.contains("Your App Assistant"),
            "embedded user skill must mention Your App Assistant"
        );
        assert!(
            USER_SKILL_BODY.contains("Unmet Intents"),
            "embedded user skill must contain Unmet Intents section"
        );
        assert!(
            USER_SKILL_BODY.contains("How to Talk to Users"),
            "embedded user skill must contain How to Talk to Users section"
        );
    }

    #[test]
    fn install_creates_user_skill() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_root = tmp.path().join(".claude").join("skills");

        install_to(&skills_root).expect("install_to should succeed");

        let user_skill_path = skills_root.join("temper-user").join("SKILL.md");
        assert!(
            user_skill_path.is_file(),
            "temper-user/SKILL.md should exist after install"
        );

        let content = fs::read_to_string(&user_skill_path).unwrap();
        assert!(
            content.starts_with("---\n"),
            "installed user skill should start with YAML frontmatter"
        );
        assert!(
            content.contains("name: temper-user"),
            "installed user skill should have skill name in frontmatter"
        );
        assert!(
            content.contains("Your App Assistant"),
            "installed user skill should contain skill body"
        );
    }

    #[test]
    fn install_overwrites_existing_and_removes_legacy() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_root = tmp.path().join(".claude").join("skills");
        fs::create_dir_all(&skills_root).unwrap();

        // Create legacy bare files
        let legacy_path = skills_root.join("temper.md");
        fs::write(&legacy_path, "old bare file").unwrap();
        let legacy_user_path = skills_root.join("temper-user.md");
        fs::write(&legacy_user_path, "old bare user file").unwrap();

        // Create old directory-style file
        let skill_dir = skills_root.join("temper-developer");
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_path = skill_dir.join("SKILL.md");
        fs::write(&skill_path, "old content").unwrap();

        install_to(&skills_root).expect("install_to should succeed on overwrite");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(
            content.contains("Temper App Builder"),
            "overwritten file should contain new content"
        );
        assert!(!legacy_path.exists(), "legacy temper.md should be removed");
        assert!(
            !legacy_user_path.exists(),
            "legacy temper-user.md should be removed"
        );
    }

    #[test]
    fn parses_genesis_app_refs() {
        let (owner, name, hash) = parse_app_ref("acme/notes@0123abcd").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(name, "notes");
        assert_eq!(hash, "0123abcd");
        assert!(parse_app_ref("acme/notes").is_err());
        assert!(parse_app_ref("acme/@0123").is_err());
        assert!(parse_app_ref("/notes@0123").is_err());
    }

    #[test]
    fn sanitizes_app_id_components() {
        assert_eq!(sanitize_id_component("Acme Labs"), "acme-labs");
        assert_eq!(sanitize_id_component("../"), "item");
    }

    #[test]
    fn normalizes_follow_policy() {
        assert_eq!(normalize_follow_policy("").unwrap(), "pinned");
        assert_eq!(normalize_follow_policy("pinned").unwrap(), "pinned");
        assert_eq!(
            normalize_follow_policy("follow-latest").unwrap(),
            "follow_latest"
        );
        assert!(normalize_follow_policy("auto").is_err());
    }
}
