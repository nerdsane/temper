//! Global skill installation for `temper install`.
//!
//! Copies the Temper App Builder skill to `~/.claude/skills/temper/SKILL.md`
//! so it auto-triggers on "build me an app" requests from any directory.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// The Temper App Builder skill body, embedded at compile time.
const SKILL_BODY: &str = include_str!("../../../../.claude/skills/temper.md");

/// YAML frontmatter required by Claude Code's skill loader.
const SKILL_FRONTMATTER: &str = "\
---
name: temper
description: \"You MUST use this skill when the user asks to build an app, create an application, make a tool, or says 'build me a X'. Temper builds apps from verified specs, not from code.\"
---
";

/// The Temper User (production chat proxy) skill body, embedded at compile time.
const USER_SKILL_BODY: &str = include_str!("../../../../.claude/skills/temper-user.md");

/// YAML frontmatter for the temper-user skill.
const USER_SKILL_FRONTMATTER: &str = "\
---
name: temper-user
description: \"Production chat proxy for a running Temper application. Translates natural language into OData API calls. Use when the user wants to interact with a running Temper app as an end user.\"
---
";

/// Install the skill to the given skills root directory (for testing with custom paths).
fn install_to(skills_root: &Path) -> Result<()> {
    let skill_dir = skills_root.join("temper");
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

    // Clean up legacy bare files from older installs
    let legacy_path = skills_root.join("temper.md");
    if legacy_path.is_file() {
        let _ = fs::remove_file(&legacy_path);
    }
    let legacy_user_path = skills_root.join("temper-user.md");
    if legacy_user_path.is_file() {
        let _ = fs::remove_file(&legacy_user_path);
    }

    println!("Installed Temper skills to {}", skills_root.display());
    println!("  - {}", skill_path.display());
    println!("  - {}", user_skill_path.display());
    println!("\nYou can now open Claude Code in any directory and say:");
    println!("  \"Build me a [your app idea]\"  (uses /temper)");
    println!("  \"/temper-user\"                 (production chat proxy)");

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

        let skill_path = skills_root.join("temper").join("SKILL.md");
        assert!(skill_path.is_file(), "SKILL.md should exist after install");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(
            content.starts_with("---\n"),
            "installed file should start with YAML frontmatter"
        );
        assert!(
            content.contains("name: temper"),
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
        let skill_dir = skills_root.join("temper");
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
}
