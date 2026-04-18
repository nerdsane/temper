//! Guard CI that fails when a metric name is registered in a
//! `*metrics*.rs` file without a corresponding emission site.
//!
//! Invoked as: `cargo run -q -p temper-server --bin check_instrumentation`.
//!
//! This is the enforcement mechanism for ADR-0052 sub-decision 2:
//! every metric registration ships with at least one emission site.
//!
//! ## Detection logic
//!
//! 1. Walk the Temper crates tree.
//! 2. Find metric registrations of the form:
//!        field_name: meter
//!            .u64_counter("temper_foo")
//!            ...
//!            .build(),
//! 3. For each `(field_name, metric_name)` pair, scan all source files
//!    for calls of the form `.<field_name>.record(`, `.<field_name>.add(`,
//!    or `.<field_name>.observe(`. One or more matches = emitted.
//! 4. Exit non-zero if any pair has zero matches.
//!
//! Metrics registered in non-struct contexts (inline singletons) skip
//! this check — they are trivially grep-able and the pattern is rare.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules", ".claude"];

/// Metric names intentionally registered but not yet emitted. Kept here
/// rather than in a frontmatter comment so the exception list is
/// grep-able and a deliberate act.
const ALLOWED_UNCALLED: &[&str] = &[
    "temper_scheduler_overdue_on_replay_total",
    "temper_handler_deadline_remaining_ms",
    "temper_handler_deadline_exceeded_total",
    "temper_wasm_epoch_tick_interval_ms",
    "temper_handler_kill_latency_ms",
];

fn main() -> ExitCode {
    let repo_root = std::env::var("CARGO_MANIFEST_DIR")
        .map(|p| PathBuf::from(p).parent().unwrap().parent().unwrap().to_path_buf())
        .unwrap_or_else(|_| std::env::current_dir().unwrap());

    let crates_root = repo_root.join("crates");
    if !crates_root.is_dir() {
        eprintln!(
            "check_instrumentation: could not find crates/ under {}",
            repo_root.display()
        );
        return ExitCode::from(2);
    }

    let mut sources: Vec<PathBuf> = Vec::new();
    collect_rust_files(&crates_root, &mut sources);

    // (field, metric_name, registered_in)
    let mut registrations: Vec<(String, String, PathBuf)> = Vec::new();
    for src in &sources {
        let Ok(text) = fs::read_to_string(src) else {
            continue;
        };
        // Skip the linter itself — it contains the pattern as strings.
        if src.ends_with("check_instrumentation.rs") {
            continue;
        }
        for (field, metric) in extract_struct_registrations(&text) {
            registrations.push((field, metric, src.clone()));
        }
    }

    // Build the combined source text once so we can grep many times cheaply.
    // Normalise all whitespace to single spaces so multi-line method chains
    // like `.field\n    .record(` match the single-line `.field.record(`
    // pattern.
    let all_text: String = {
        let raw: String = sources
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n");
        raw.split_whitespace().collect::<Vec<_>>().join(" ")
    };

    let mut unemitted: Vec<(String, String, PathBuf)> = Vec::new();
    // Dedup by metric name since a metric is one logical thing even if a
    // field name collides across unrelated structs.
    let mut by_metric: BTreeMap<String, (String, PathBuf)> = BTreeMap::new();
    for (field, metric, path) in registrations {
        by_metric.insert(metric, (field, path));
    }

    for (metric, (field, path)) in &by_metric {
        if ALLOWED_UNCALLED.contains(&metric.as_str()) {
            continue;
        }
        // Accept both `.field.record(` and `.field .record(` since the
        // whitespace normalizer leaves one space between method-chain
        // elements on separate source lines.
        let call_patterns = [
            format!(".{}.record(", field),
            format!(".{} .record(", field),
            format!(".{}.add(", field),
            format!(".{} .add(", field),
            format!(".{}.observe(", field),
            format!(".{} .observe(", field),
        ];
        let emitted = call_patterns.iter().any(|p| all_text.contains(p));
        if !emitted {
            unemitted.push((metric.clone(), field.clone(), path.clone()));
        }
    }

    if unemitted.is_empty() {
        println!(
            "check_instrumentation: OK — {} registered metrics all have ≥1 emission site (excluding {} allowed exceptions).",
            by_metric.len(),
            ALLOWED_UNCALLED.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "check_instrumentation: FAIL — {} metric(s) registered with no emission site. \
             Wire an emission via self.<field>.record/add/observe, or add the metric name \
             to ALLOWED_UNCALLED in src/bin/check_instrumentation.rs with a reason:",
            unemitted.len()
        );
        for (metric, field, path) in unemitted {
            eprintln!("  - {metric} (field `{field}`, in {})", path.display());
        }
        ExitCode::from(1)
    }
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if SKIP_DIRS.contains(&name) {
            continue;
        }
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Extract `(field_name, metric_name)` pairs from patterns like:
///     field_name: meter
///         .u64_counter("temper_foo")
///
/// We scan line-by-line for the registrar invocation, then walk backwards
/// to the nearest `<ident>: meter` introducer.
fn extract_struct_registrations(text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let registrars = [
        ".u64_counter(\"",
        ".f64_histogram(\"",
        ".u64_histogram(\"",
        ".u64_gauge(\"",
        ".f64_gauge(\"",
    ];
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        for registrar in &registrars {
            if let Some(pos) = line.find(registrar) {
                let after = &line[pos + registrar.len()..];
                let end = match after.find('"') {
                    Some(e) => e,
                    None => continue,
                };
                let metric = &after[..end];
                if !metric.starts_with("temper_") {
                    continue;
                }
                // Walk backward up to 10 lines looking for `<ident>: meter`
                // or `<ident>: global::meter(...)`.
                let mut found_field: Option<String> = None;
                for back in 1..=10 {
                    if i < back {
                        break;
                    }
                    let prev = lines[i - back].trim();
                    if let Some(f) = prev.strip_suffix(": meter") {
                        found_field = Some(f.trim().to_string());
                        break;
                    }
                    // Patterns like: `    field: meter` already handled above.
                    // Also accept: `field: meter\n\t\t.u64_counter(...)` variants.
                }
                if let Some(field) = found_field {
                    out.push((field, metric.to_string()));
                }
            }
        }
    }
    out
}
