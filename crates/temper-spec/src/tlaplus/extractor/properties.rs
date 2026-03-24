use super::{Invariant, LivenessProperty};

pub(super) fn extract_invariants(source: &str) -> Vec<Invariant> {
    extract_named_formulas(
        source,
        |line| line.starts_with("\\*") && line.contains("Safety Invariant"),
        |line| line.starts_with("\\*") && line.contains("Liveness"),
        |line| line.starts_with("SafetyInvariant"),
        true,
    )
    .into_iter()
    .map(|formula| Invariant {
        name: formula.name,
        expr: formula.expr,
    })
    .collect()
}

pub(super) fn extract_liveness(source: &str) -> Vec<LivenessProperty> {
    extract_named_formulas(
        source,
        |line| line.starts_with("\\*") && line.contains("Liveness"),
        |line| {
            line.starts_with("\\*")
                && (line.contains("Specification") || line.contains("Model checking"))
        },
        |_| false,
        false,
    )
    .into_iter()
    .map(|formula| LivenessProperty {
        name: formula.name,
        expr: formula.expr,
    })
    .collect()
}

struct NamedFormula {
    name: String,
    expr: String,
}

fn extract_named_formulas(
    source: &str,
    starts_section: impl Fn(&str) -> bool,
    ends_section: impl Fn(&str) -> bool,
    skips_definition: impl Fn(&str) -> bool,
    blank_line_ends_formula: bool,
) -> Vec<NamedFormula> {
    let mut formulas = Vec::new();
    let mut current_name = None;
    let mut current_expr = String::new();
    let mut in_section = false;

    for line in source.lines() {
        let trimmed = line.trim();

        if starts_section(trimmed) {
            in_section = true;
            continue;
        }

        if in_section && ends_section(trimmed) {
            flush_formula(&mut formulas, &mut current_name, &mut current_expr);
            in_section = false;
            continue;
        }

        if !in_section {
            continue;
        }

        if is_named_definition(trimmed, &skips_definition) {
            flush_formula(&mut formulas, &mut current_name, &mut current_expr);
            start_formula(trimmed, &mut current_name, &mut current_expr);
            continue;
        }

        if current_name.is_some() && ends_formula(trimmed, blank_line_ends_formula) {
            flush_formula(&mut formulas, &mut current_name, &mut current_expr);
            continue;
        }

        if current_name.is_some() {
            current_expr.push_str(trimmed);
            current_expr.push('\n');
        }
    }

    flush_formula(&mut formulas, &mut current_name, &mut current_expr);
    formulas
}

fn is_named_definition(trimmed: &str, skips_definition: &impl Fn(&str) -> bool) -> bool {
    trimmed.contains(" ==") && !trimmed.starts_with("\\*") && !skips_definition(trimmed)
}

fn start_formula(line: &str, current_name: &mut Option<String>, current_expr: &mut String) {
    let parts: Vec<&str> = line.splitn(2, "==").collect();
    if parts.len() == 2 {
        *current_name = Some(parts[0].trim().to_string());
        current_expr.clear();
        current_expr.push_str(parts[1].trim());
        current_expr.push('\n');
    }
}

fn ends_formula(trimmed: &str, blank_line_ends_formula: bool) -> bool {
    trimmed.is_empty()
        || (blank_line_ends_formula && trimmed.contains(" ==") && !trimmed.starts_with("/\\"))
}

fn flush_formula(
    formulas: &mut Vec<NamedFormula>,
    current_name: &mut Option<String>,
    current_expr: &mut String,
) {
    if let Some(name) = current_name.take() {
        formulas.push(NamedFormula {
            name,
            expr: current_expr.trim().to_string(),
        });
        current_expr.clear();
    }
}
