//! Git-based app sources — clone external repos and register them as app directories.
//!
//! Reads the `TEMPER_APP_SOURCES` environment variable at startup. Each entry
//! is a git URL with an optional `@ref` suffix (defaults to `main`). Repos are
//! shallow-cloned into a host-provided cache directory and registered via
//! [`super::add_os_apps_dir`].
//!
//! See ADR-0042 for the full rationale.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A parsed git app source entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitAppSource {
    /// Git clone URL (HTTPS or SSH).
    pub url: String,
    /// Git ref to checkout (branch, tag, or commit SHA). Defaults to `"main"`.
    pub git_ref: String,
}

/// Parse a `TEMPER_APP_SOURCES` value into a list of [`GitAppSource`] entries.
///
/// Format: `<url>[@<ref>][,<url>[@<ref>],...]`
///
/// Empty or whitespace-only entries are skipped.
pub fn parse_app_sources(value: &str) -> Vec<GitAppSource> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|entry| {
            // Split on the LAST '@' to handle SSH URLs like git@github.com:...
            let (url, git_ref) = if let Some(at_pos) = entry.rfind('@') {
                let before = &entry[..at_pos];
                let after = &entry[at_pos + 1..];
                // The ref shouldn't contain ':' (SSH URL separator).
                // If 'after' looks like a ref (no '/' or ':'), treat it as one.
                if !after.is_empty()
                    && !after.contains('/')
                    && !after.contains(':')
                    && !before.is_empty()
                {
                    (before.to_string(), after.to_string())
                } else {
                    (entry.to_string(), "main".to_string())
                }
            } else {
                (entry.to_string(), "main".to_string())
            };
            GitAppSource { url, git_ref }
        })
        .collect()
}

/// Extract a repository name from a git URL.
///
/// Examples:
/// - `https://github.com/arni-labs/palimpsest.git` → `palimpsest`
/// - `git@github.com:arni-labs/palimpsest.git` → `palimpsest`
/// - `https://github.com/arni-labs/palimpsest` → `palimpsest`
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let cleaned = url.trim_end_matches('/').trim_end_matches(".git");
    // Take the last path segment (works for both HTTPS and SSH URLs).
    let name = cleaned.rsplit('/').next().or_else(|| {
        // SSH format: git@host:org/repo — split on ':'
        cleaned.rsplit(':').next()
    })?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Clone or update a single git repository into `cache_dir/{repo_name}`.
///
/// - First run: `git clone --depth 1 --branch {ref} {url} {path}`
/// - Subsequent runs: `git fetch origin {ref}` + `git checkout FETCH_HEAD`
///
/// Returns the path to the cloned repo root.
pub fn sync_git_source(source: &GitAppSource, cache_dir: &Path) -> Result<PathBuf, String> {
    let name = repo_name_from_url(&source.url)
        .ok_or_else(|| format!("cannot extract repo name from URL: {}", source.url))?;
    let repo_dir = cache_dir.join(&name);

    if repo_dir.join(".git").is_dir() {
        // Update existing clone.
        tracing::info!(
            repo = %name,
            git_ref = %source.git_ref,
            "Updating git app source"
        );
        run_git(
            &["fetch", "--depth", "1", "origin", &source.git_ref],
            &repo_dir,
        )?;
        reset_worktree_to_fetch_head(&repo_dir)?;
    } else {
        // Fresh clone.
        tracing::info!(
            repo = %name,
            url = %source.url,
            git_ref = %source.git_ref,
            "Cloning git app source"
        );
        std::fs::create_dir_all(cache_dir)
            .map_err(|e| format!("failed to create cache dir {}: {e}", cache_dir.display()))?;
        run_git(
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                &source.git_ref,
                &source.url,
                &repo_dir.to_string_lossy(),
            ],
            cache_dir,
        )?;
    }

    Ok(repo_dir)
}

fn reset_worktree_to_fetch_head(repo_dir: &Path) -> Result<(), String> {
    run_git(&["reset", "--hard", "FETCH_HEAD"], repo_dir)?;
    run_git(&["clean", "-ffdx"], repo_dir)
}

/// Parse `TEMPER_APP_SOURCES`, sync all repos, and register each as an app directory.
///
/// Returns the list of repository names that were successfully synced.
/// If `TEMPER_APP_SOURCES` is not set, returns an empty list (not an error).
pub fn sync_and_register_git_sources(cache_dir: &Path) -> Result<Vec<String>, String> {
    // determinism-ok: env var read at startup for configuration
    let sources_value = match std::env::var("TEMPER_APP_SOURCES") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(Vec::new()),
    };

    let sources = parse_app_sources(&sources_value);
    let mut synced = Vec::new();

    for source in &sources {
        match sync_git_source(source, cache_dir) {
            Ok(repo_dir) => {
                let name = repo_name_from_url(&source.url).unwrap_or_default();
                super::add_os_apps_dir(repo_dir);
                synced.push(name);
            }
            Err(e) => {
                tracing::warn!(
                    url = %source.url,
                    error = %e,
                    "Failed to sync git app source — skipping"
                );
            }
        }
    }

    Ok(synced)
}

/// Run a git command and return Ok(()) on success, Err(stderr) on failure.
fn run_git(args: &[&str], working_dir: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.first().unwrap_or(&"")))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "git {} failed (exit {}): {}",
            args.first().unwrap_or(&""),
            output.status.code().unwrap_or(-1),
            stderr.trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_app_sources ──────────────────────────────────────────────

    #[test]
    fn parse_single_url_no_ref() {
        let sources = parse_app_sources("https://github.com/arni-labs/palimpsest.git");
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].url,
            "https://github.com/arni-labs/palimpsest.git"
        );
        assert_eq!(sources[0].git_ref, "main");
    }

    #[test]
    fn parse_single_url_with_ref() {
        let sources = parse_app_sources("https://github.com/arni-labs/palimpsest.git@v2");
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].url,
            "https://github.com/arni-labs/palimpsest.git"
        );
        assert_eq!(sources[0].git_ref, "v2");
    }

    #[test]
    fn parse_multiple_urls() {
        let sources =
            parse_app_sources("https://github.com/a/one.git,https://github.com/b/two.git@dev");
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].url, "https://github.com/a/one.git");
        assert_eq!(sources[0].git_ref, "main");
        assert_eq!(sources[1].url, "https://github.com/b/two.git");
        assert_eq!(sources[1].git_ref, "dev");
    }

    #[test]
    fn parse_with_whitespace() {
        let sources = parse_app_sources(
            "  https://github.com/a/one.git , https://github.com/b/two.git@dev  ",
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].url, "https://github.com/a/one.git");
        assert_eq!(sources[1].url, "https://github.com/b/two.git");
    }

    #[test]
    fn parse_empty_string() {
        let sources = parse_app_sources("");
        assert!(sources.is_empty());
    }

    #[test]
    fn parse_only_whitespace_and_commas() {
        let sources = parse_app_sources(" , , ");
        assert!(sources.is_empty());
    }

    #[test]
    fn parse_ssh_url_no_ref() {
        let sources = parse_app_sources("git@github.com:arni-labs/palimpsest.git");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "git@github.com:arni-labs/palimpsest.git");
        assert_eq!(sources[0].git_ref, "main");
    }

    #[test]
    fn parse_ssh_url_with_ref() {
        let sources = parse_app_sources("git@github.com:arni-labs/palimpsest.git@v3");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "git@github.com:arni-labs/palimpsest.git");
        assert_eq!(sources[0].git_ref, "v3");
    }

    // ── repo_name_from_url ─────────────────────────────────────────────

    #[test]
    fn repo_name_https_with_git_suffix() {
        assert_eq!(
            repo_name_from_url("https://github.com/arni-labs/palimpsest.git"),
            Some("palimpsest".to_string())
        );
    }

    #[test]
    fn repo_name_https_without_git_suffix() {
        assert_eq!(
            repo_name_from_url("https://github.com/arni-labs/palimpsest"),
            Some("palimpsest".to_string())
        );
    }

    #[test]
    fn repo_name_https_trailing_slash() {
        assert_eq!(
            repo_name_from_url("https://github.com/arni-labs/palimpsest.git/"),
            Some("palimpsest".to_string())
        );
    }

    #[test]
    fn repo_name_ssh() {
        assert_eq!(
            repo_name_from_url("git@github.com:arni-labs/palimpsest.git"),
            Some("palimpsest".to_string())
        );
    }

    #[test]
    fn repo_name_empty() {
        assert_eq!(repo_name_from_url(""), None);
    }

    // ── sync_git_source (integration) ──────────────────────────────────

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("temper-{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    #[ignore] // requires network + git credentials for private repo
    fn sync_clones_real_repo() {
        let tmp = make_temp_dir("git-clone");
        let source = GitAppSource {
            url: "https://github.com/arni-labs/palimpsest.git".to_string(),
            git_ref: "main".to_string(),
        };
        let result = sync_git_source(&source, &tmp);
        assert!(result.is_ok(), "sync failed: {:?}", result.err());
        let repo_dir = result.unwrap();
        assert!(
            repo_dir.join(".git").is_dir(),
            "cloned dir should have .git"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    #[ignore] // requires network + git credentials for private repo
    fn sync_update_existing_clone() {
        let tmp = make_temp_dir("git-update");
        let source = GitAppSource {
            url: "https://github.com/arni-labs/palimpsest.git".to_string(),
            git_ref: "main".to_string(),
        };
        let first = sync_git_source(&source, &tmp);
        assert!(first.is_ok(), "first sync failed: {:?}", first.err());
        let second = sync_git_source(&source, &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            second.is_ok(),
            "second sync (update) failed: {:?}",
            second.err()
        );
    }

    #[test]
    fn sync_update_removes_untracked_stale_files_from_cached_clone() {
        let remote = make_temp_dir("git-local-remote");
        let cache = make_temp_dir("git-cache");

        run_git(&["init", "--initial-branch", "main"], &remote).expect("init remote repo");
        let app_dir = remote.join("os-apps/katagami-curation");
        std::fs::create_dir_all(&app_dir).expect("create app dir");
        std::fs::write(app_dir.join("app.toml"), "name = \"katagami-curation\"\n")
            .expect("write app");
        run_git(&["add", "."], &remote).expect("git add");
        run_git(
            &[
                "-c",
                "user.name=Temper Test",
                "-c",
                "user.email=temper@example.test",
                "commit",
                "--no-verify",
                "-m",
                "initial app",
            ],
            &remote,
        )
        .expect("commit");

        let source = GitAppSource {
            url: remote.to_string_lossy().into_owned(),
            git_ref: "main".to_string(),
        };
        let repo_dir = sync_git_source(&source, &cache).expect("initial sync");

        let stale_reactions = repo_dir.join("os-apps/katagami-curation/reactions/reactions.toml");
        std::fs::create_dir_all(stale_reactions.parent().expect("stale parent"))
            .expect("create stale reactions dir");
        std::fs::write(&stale_reactions, "[[reaction]]\n").expect("write stale reactions");
        assert!(
            stale_reactions.exists(),
            "test setup should create stale file"
        );

        let updated_repo_dir = sync_git_source(&source, &cache).expect("second sync");
        assert_eq!(updated_repo_dir, repo_dir);
        assert!(
            !stale_reactions.exists(),
            "sync must remove stale untracked files from cached git app sources"
        );

        let _ = std::fs::remove_dir_all(&remote);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    #[ignore] // requires network access
    fn sync_bad_url_returns_error() {
        let tmp = make_temp_dir("git-bad-url");
        let source = GitAppSource {
            url: "https://github.com/nonexistent/repo-that-does-not-exist-12345.git".to_string(),
            git_ref: "main".to_string(),
        };
        let result = sync_git_source(&source, &tmp);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(result.is_err());
    }
}
