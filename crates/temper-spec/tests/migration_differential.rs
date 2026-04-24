//! ADR-0046 migration differential.
//!
//! For each spec migrated from `[[integration]]` to `[[action.triggers]]`,
//! fetch the pre-migration TOML from git and parse both. After post-parse
//! expansion the new form synthesizes Integration entries; the two vectors
//! must be structurally equal (sorted by name for order-independence).
//!
//! Catches the class of bug where a top-level integration field (e.g. `prompt`)
//! is silently dropped during migration.
//!
//! Scope: this proves `Integration`-level preservation — the runtime-facing
//! dispatch record. ActionTrigger-only metadata with no Integration analogue
//! (resolve_target, params_from, trigger-level guards) is out of scope;
//! those apply to entity-kind triggers and are covered by the reaction
//! synthesis tests. Assumes every listed migration SHA has exactly one parent
//! (true for all commits in MIGRATIONS as of ship).

use std::collections::BTreeMap;
use std::process::Command;

use temper_spec::automaton::{Integration, parse_automaton};

/// (repo_worktree, migration_commit_sha, spec_path_relative_to_repo_root)
const MIGRATIONS: &[(&str, &str, &str)] = &[
    // --- temper repo ---
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "53e2304",
        "reference-apps/weather-tracker/specs/weather_report.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "c7cf6a2",
        "os-apps/temper-agent/specs/cron_job.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "c7cf6a2",
        "os-apps/temper-agent/specs/cron_scheduler.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "c7cf6a2",
        "os-apps/temper-agent/specs/heartbeat_monitor.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "c7cf6a2",
        "reference-apps/crucible/specs/crucible_scheduler.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "c7cf6a2",
        "reference-apps/crucible/specs/session_schedule.ioa.toml",
    ),
    // ad06abd (not 15c8e87): get pre-migration content, not post-prompt-fix.
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "ad06abd",
        "os-apps/evolution/evolution_run.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "ad06abd",
        "os-apps/intent-discovery/specs/intent_discovery.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "ad06abd",
        "os-apps/temper-agent/specs/temper_agent.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/temper-action-triggers",
        "ad06abd",
        "os-apps/temper-channels/specs/channel.ioa.toml",
    ),
    // --- openpaw repo — 7a644954 bulk migration ---
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-agent/specs/capability_request.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-agent/specs/cron_job.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-agent/specs/plan_review.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-agent/specs/session.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-autoreason/specs/tournament.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-channels/specs/channel.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-consilium/specs/deliberation.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-foresight/specs/foresight_model.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-foresight/specs/projection.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-fs/specs/workspace.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-heal/specs/alert_cycle.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-ingest/specs/webhook_event.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "7a644954",
        "os-apps/paw-wiki/specs/wiki_job.ioa.toml",
    ),
    // --- openpaw batch 2 (trigger-keyed script fix) — commit 563c6048 ---
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "563c6048",
        "os-apps/paw-research/specs/web_query.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "563c6048",
        "os-apps/paw-managed-agents/specs/managed_session.ioa.toml",
    ),
    (
        "/Users/seshendranalla/Development/openpaw-action-triggers",
        "563c6048",
        "os-apps/paw-managed-agents/specs/managed_agent.ioa.toml",
    ),
];

fn git_show(repo_root: &str, sha: &str, path: &str) -> String {
    git_show_inner(repo_root, sha, path, None)
}

fn git_show_inner(
    repo_root: &str,
    sha: &str,
    path: &str,
    git_dir_override: Option<&str>,
) -> String {
    let mut cmd = Command::new("git");
    if let Some(git_dir) = git_dir_override {
        cmd.env("GIT_DIR", git_dir);
    }
    sanitize_git_env(&mut cmd);
    let out = cmd
        .args(["-C", repo_root, "show", &format!("{sha}^:{path}")])
        .output()
        .expect("git show");
    assert!(
        out.status.success(),
        "git show {sha}^:{path} in {repo_root} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

fn sanitize_git_env(cmd: &mut Command) {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(key);
    }
}

fn read_current(repo_root: &str, path: &str) -> String {
    std::fs::read_to_string(format!("{repo_root}/{path}"))
        .unwrap_or_else(|e| panic!("read {repo_root}/{path}: {e}"))
}

/// Integrations intentionally dropped (not just migrated) after the initial
/// conversion. These are expected to be missing from the post-migration spec
/// — the differential treats their absence as correct, not a regression.
///
/// Keyed by `(path, inner_name(trigger))`.
const INTENTIONALLY_DROPPED: &[(&str, &str)] = &[
    // paw-agent/session.call_llm — monolithic llm_caller retired after the
    // session flow was split into prepare_context → call_provider →
    // apply_provider_response. Deleted in commit ce3dddcb.
    ("os-apps/paw-agent/specs/session.ioa.toml", "call_llm"),
];

/// Return the inner name — strips the `__trigger__:{action}:` prefix that
/// the expander adds when synthesizing Integrations from `[[action.triggers]]`
/// (parser.rs:109). Lets us compare old-form and new-form entries by the
/// human-facing trigger name regardless of dispatch namespace.
fn inner_name(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("__trigger__:") {
        rest.split_once(':').map(|(_, n)| n).unwrap_or(rest)
    } else {
        name
    }
}

/// Index by dispatch key — the `trigger` field stripped of any `__trigger__:`
/// prefix. This is what the invoking action's `effect = trigger X` resolves
/// against at runtime, and the invariant we care about preserving across
/// migration. The integration's `name` field was a separate human-readable
/// label that ADR-0046 collapses into the dispatch key.
fn index(integrations: &[Integration]) -> BTreeMap<String, &Integration> {
    integrations
        .iter()
        .map(|i| (inner_name(&i.trigger).to_string(), i))
        .collect()
}

fn assert_equiv(path: &str, old: &Integration, new: &Integration) {
    // Trigger (dispatch key) is the invariant — this is what `effect = "trigger X"`
    // resolves against and what the runtime routes by. The `name` field was a
    // separate human-readable label pre-ADR-0046; the new form collapses both
    // into a single dispatch-keyed name, so we deliberately don't compare names.
    assert_eq!(
        inner_name(&old.trigger),
        inner_name(&new.trigger),
        "{path}: trigger inner name"
    );
    assert_eq!(
        old.integration_type, new.integration_type,
        "{path}: type name={}",
        old.name
    );
    assert_eq!(old.module, new.module, "{path}: module name={}", old.name);
    assert_eq!(
        old.on_success, new.on_success,
        "{path}: on_success name={}",
        old.name
    );
    assert_eq!(
        old.on_failure, new.on_failure,
        "{path}: on_failure name={}",
        old.name
    );
    assert_eq!(old.llm, new.llm, "{path}: llm name={}", old.name);
    assert_config_equiv(path, inner_name(&old.trigger), old, new);
}

fn assert_config_equiv(path: &str, trigger_name: &str, old: &Integration, new: &Integration) {
    let old_cfg = normalize_old_config_for_expected_evolution(path, trigger_name, &old.config);
    let new_cfg = normalize_new_config_for_expected_evolution(path, trigger_name, &new.config);
    let allowed_extra = allowed_extra_config_keys(path, trigger_name);

    if allowed_extra.is_empty() {
        let old_keys: Vec<&String> = old_cfg.keys().collect();
        let new_keys: Vec<&String> = new_cfg.keys().collect();
        assert_eq!(
            old_keys, new_keys,
            "{path}: config keys differ for {}: old={:?} new={:?}",
            old.name, old_keys, new_keys
        );
        for (k, v) in &old_cfg {
            assert_eq!(
                Some(v),
                new_cfg.get(k),
                "{path}: config[{k}] name={}",
                old.name
            );
        }
        return;
    }

    for (k, v) in &old_cfg {
        assert_eq!(
            Some(v),
            new_cfg.get(k),
            "{path}: config[{k}] name={}",
            old.name
        );
    }

    let mut unexpected_new_keys: Vec<&str> = Vec::new();
    for key in new_cfg.keys() {
        if old_cfg.contains_key(key) {
            continue;
        }
        if allowed_extra.contains(&key.as_str()) {
            continue;
        }
        unexpected_new_keys.push(key.as_str());
    }
    assert!(
        unexpected_new_keys.is_empty(),
        "{path}: unexpected config keys for {}: {:?}",
        old.name,
        unexpected_new_keys
    );
}

fn normalize_old_config_for_expected_evolution(
    path: &str,
    trigger_name: &str,
    config: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut normalized = config.clone();
    match (path, trigger_name) {
        ("os-apps/paw-agent/specs/session.ioa.toml", "call_provider") => {
            normalized.remove("api_key");
            if let Some(openai_api_url) = normalized.remove("openai_api_url") {
                normalized.insert("openai_codex_api_url".to_string(), openai_api_url);
            }
        }
        ("os-apps/paw-agent/specs/session.ioa.toml", "compact_context") => {
            normalized.remove("api_key");
        }
        _ => {}
    }
    normalized
}

fn normalize_new_config_for_expected_evolution(
    path: &str,
    trigger_name: &str,
    config: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut normalized = config.clone();
    match (path, trigger_name) {
        ("os-apps/paw-agent/specs/session.ioa.toml", "call_provider" | "compact_context") => {
            normalized.remove("api_key");
        }
        _ => {}
    }
    normalized
}

fn allowed_extra_config_keys(path: &str, trigger_name: &str) -> &'static [&'static str] {
    match (path, trigger_name) {
        ("os-apps/paw-agent/specs/session.ioa.toml", "call_provider") => {
            &["openai_api_key", "openai_api_url"]
        }
        ("os-apps/paw-agent/specs/session.ioa.toml", "compact_context") => &[
            "anthropic_api_url",
            "openai_api_key",
            "openai_api_url",
            "openai_codex_api_url",
            "openrouter_api_url",
        ],
        _ => &[],
    }
}

#[test]
fn all_migrations_preserve_integrations() {
    let mut failures: Vec<String> = Vec::new();

    for (repo, sha, path) in MIGRATIONS {
        let old_src = git_show(repo, sha, path);
        let new_src = read_current(repo, path);

        let old = match parse_automaton(&old_src) {
            Ok(a) => a,
            Err(e) => {
                failures.push(format!("{path}: old parse failed: {e:?}"));
                continue;
            }
        };
        let new = match parse_automaton(&new_src) {
            Ok(a) => a,
            Err(e) => {
                failures.push(format!("{path}: new parse failed: {e:?}"));
                continue;
            }
        };

        let old_idx = index(&old.integrations);
        let new_idx = index(&new.integrations);

        let old_names: Vec<&String> = old_idx.keys().collect();
        let new_names: Vec<&String> = new_idx.keys().collect();

        // New may be a superset if any inline triggers were never [[integration]]
        // (e.g. entity-kind triggers); but every pre-existing integration must persist
        // unless explicitly listed in INTENTIONALLY_DROPPED.
        for name in &old_names {
            match new_idx.get(*name) {
                None => {
                    if INTENTIONALLY_DROPPED
                        .iter()
                        .any(|(p, n)| *p == *path && *n == name.as_str())
                    {
                        eprintln!(
                            "  {path}: note — integration '{name}' intentionally dropped post-migration"
                        );
                    } else {
                        failures.push(format!("{path}: integration '{name}' dropped by migration"));
                    }
                }
                Some(new_i) => {
                    let old_i = old_idx[*name];
                    let result = std::panic::catch_unwind(|| assert_equiv(path, old_i, new_i));
                    if let Err(e) = result {
                        let msg = e
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                            .unwrap_or_else(|| "unknown panic".into());
                        failures.push(msg);
                    }
                }
            }
        }

        // Surface extras for visibility (not a failure — may be legit entity-kind additions).
        for name in &new_names {
            if !old_idx.contains_key(*name) {
                eprintln!(
                    "  {path}: note — new integration '{name}' appeared (likely entity-kind or newly synthesized)"
                );
            }
        }
    }

    if !failures.is_empty() {
        panic!(
            "Migration differential FAILED — {} mismatch(es):\n  - {}",
            failures.len(),
            failures.join("\n  - ")
        );
    }
}

/// Guards the expander's naming scheme (parser.rs:109). If the
/// `__trigger__:{action}:{name}` prefix is ever dropped, the main
/// differential would silently collapse both sides to the same raw name.
/// This test fails loudly in that case.
#[test]
fn expander_namespaces_synthesized_integrations() {
    let src = r#"
[automaton]
name = "Probe"
states = ["Ready"]
initial = "Ready"

[[action]]
name = "Fire"
kind = "input"
from = ["Ready"]
to = "Ready"

[[action.triggers]]
name = "my_trigger"
kind = "wasm"
module = "probe_module"
on_success = "Fire"
"#;
    let a = parse_automaton(src).expect("parse");
    assert_eq!(
        a.integrations.len(),
        1,
        "one synthesized Integration expected"
    );
    assert_eq!(
        a.integrations[0].name, "__trigger__:Fire:my_trigger",
        "expander must namespace synthesized names as `__trigger__:{{action}}:{{name}}`"
    );
}

#[test]
fn git_show_ignores_inherited_git_dir_from_other_repo() {
    let repo = "/Users/seshendranalla/Development/openpaw-action-triggers";
    let wrong_git_dir = "/Users/seshendranalla/Development/temper-action-triggers/.git";
    let path = "os-apps/paw-agent/specs/capability_request.ioa.toml";

    let clean = git_show(repo, "7a644954", path);
    let contaminated = git_show_inner(repo, "7a644954", path, Some(wrong_git_dir));

    assert_eq!(clean, contaminated);
}
