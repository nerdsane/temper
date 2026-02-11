//! Global skill installation for `temper install`.
//!
//! Copies the Temper App Builder skill to `~/.claude/skills/temper.md`
//! so it auto-triggers on "build me an app" requests from any directory.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

/// The Temper App Builder skill, embedded at compile time.
const SKILL_CONTENT: &str = include_str!("../../../../.claude/skills/temper.md");

/// Install the skill to the given directory (for testing with custom paths).
fn install_to(skills_dir: &Path) -> Result<()> {
    fs::create_dir_all(skills_dir)
        .with_context(|| format!("Failed to create directory: {}", skills_dir.display()))?;

    let skill_path = skills_dir.join("temper.md");
    fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("Failed to write {}", skill_path.display()))?;

    println!("Installed Temper skill to {}", skill_path.display());
    println!("\nYou can now open Claude Code in any directory and say:");
    println!("  \"Build me a [your app idea]\"");

    Ok(())
}

/// Run the `temper install` command.
///
/// Copies the embedded Temper App Builder skill to `~/.claude/skills/temper.md`.
pub fn run() -> Result<()> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let skills_dir = home.join(".claude").join("skills");
    install_to(&skills_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_content_is_embedded() {
        assert!(
            SKILL_CONTENT.contains("Temper App Builder"),
            "embedded skill must mention Temper App Builder"
        );
        assert!(
            SKILL_CONTENT.contains("Interview Protocol"),
            "embedded skill must contain Interview Protocol section"
        );
        assert!(
            SKILL_CONTENT.contains("IOA TOML Spec Format"),
            "embedded skill must contain IOA TOML Spec Format section"
        );
    }

    #[test]
    fn install_creates_skill_file() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_dir = tmp.path().join(".claude").join("skills");

        install_to(&skills_dir).expect("install_to should succeed");

        let skill_path = skills_dir.join("temper.md");
        assert!(skill_path.is_file(), "temper.md should exist after install");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(
            content.contains("Temper App Builder"),
            "installed file should contain Temper App Builder"
        );
    }

    #[test]
    fn install_overwrites_existing_file() {
        let tmp = tempfile::tempdir().expect("failed to create temp dir");
        let skills_dir = tmp.path().join(".claude").join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        let skill_path = skills_dir.join("temper.md");
        fs::write(&skill_path, "old content").unwrap();

        install_to(&skills_dir).expect("install_to should succeed on overwrite");

        let content = fs::read_to_string(&skill_path).unwrap();
        assert!(
            content.contains("Temper App Builder"),
            "overwritten file should contain new content"
        );
        assert!(
            !content.contains("old content"),
            "overwritten file should not contain old content"
        );
    }
}
