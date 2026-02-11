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

/// Install the skill to the given skills root directory (for testing with custom paths).
fn install_to(skills_root: &Path) -> Result<()> {
    let skill_dir = skills_root.join("temper");
    fs::create_dir_all(&skill_dir)
        .with_context(|| format!("Failed to create directory: {}", skill_dir.display()))?;

    let skill_path = skill_dir.join("SKILL.md");
    let content = format!("{SKILL_FRONTMATTER}{SKILL_BODY}");
    fs::write(&skill_path, &content)
        .with_context(|| format!("Failed to write {}", skill_path.display()))?;

    // Clean up legacy bare file from older installs
    let legacy_path = skills_root.join("temper.md");
    if legacy_path.is_file() {
        let _ = fs::remove_file(&legacy_path);
    }

    println!("Installed Temper skill to {}", skill_path.display());
    println!("\nYou can now open Claude Code in any directory and say:");
    println!("  \"Build me a [your app idea]\"");

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
    fn install_overwrites_existing_and_removes_legacy() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_root = tmp.path().join(".claude").join("skills");
        fs::create_dir_all(&skills_root).unwrap();

        // Create legacy bare file
        let legacy_path = skills_root.join("temper.md");
        fs::write(&legacy_path, "old bare file").unwrap();

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
        assert!(
            !legacy_path.exists(),
            "legacy temper.md should be removed"
        );
    }
}
