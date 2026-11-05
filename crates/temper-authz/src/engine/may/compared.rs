//! Which attributes a policy compares with each other.

use serde_json::Value as Json;

use super::json::single;

/// An attribute a policy reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// `resource.<field>`.
    Resource(String),
    /// `context.<key>`.
    Context(String),
}

/// Add each comparison under `expr` that reads two attributes or more, as the
/// list of what it reads. Everything under a comparison counts, whichever
/// branch of an `if` it sits in: this says what can meet, not what does.
pub(super) fn comparisons(expr: &Json, found: &mut Vec<Vec<Read>>) {
    match single(expr) {
        Some(("==" | "!=" | "in" | "contains" | "containsAll" | "containsAny", arg)) => {
            let mut read = Vec::new();
            reads(arg, &mut read);
            if read.len() > 1 {
                found.push(read);
            }
        }
        _ => each_part(expr, |part| comparisons(part, found)),
    }
}

fn reads(expr: &Json, found: &mut Vec<Read>) {
    if let Some((".", arg)) = single(expr)
        && let (Some(("Var", var)), Some(attr)) = (single(&arg["left"]), arg["attr"].as_str())
    {
        match var.as_str() {
            Some("resource") => found.push(Read::Resource(attr.to_string())),
            Some("context") => found.push(Read::Context(attr.to_string())),
            _ => {}
        }
    } else {
        each_part(expr, |part| reads(part, found));
    }
}

fn each_part(expr: &Json, mut visit: impl FnMut(&Json)) {
    match expr {
        Json::Object(object) => object.values().for_each(&mut visit),
        Json::Array(items) => items.iter().for_each(&mut visit),
        _ => {}
    }
}
