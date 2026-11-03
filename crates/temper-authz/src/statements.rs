//! Cedar statements of a policy text, compared by meaning rather than bytes.
//!
//! A statement's identity is its parsed EST JSON, so the same policy written
//! with other whitespace, line endings or comments is the same statement.
//! Removal cuts each statement's source span and keeps every other byte, so
//! comments and layout around the remaining statements survive.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use cedar_policy::{Policy, Template};
use cedar_policy_core::parser::cst;
use cedar_policy_core::parser::text_to_cst::parse_policies;

use crate::AuthzError;

/// The parsed identity of one Cedar statement (static policy or template).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatementKey(String);

/// Every statement of `text` with its source span, in source order.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn policy_statements(text: &str) -> Result<Vec<(Range<usize>, StatementKey)>, AuthzError> {
    statement_spans(text)?
        .into_iter()
        .map(|(span, _)| Ok((span.clone(), statement_key(&text[span])?)))
        .collect()
}

/// Every statement's source span, and whether it already carries a source
/// label, read from the syntax tree alone.
fn statement_spans(text: &str) -> Result<Vec<(Range<usize>, bool)>, AuthzError> {
    let cst = parse_policies(text).map_err(|error| AuthzError::PolicyParse(error.to_string()))?;
    let Some(policies) = cst.node else {
        return Ok(Vec::new());
    };
    let mut spans = Vec::with_capacity(policies.0.len());
    for policy in &policies.0 {
        let loc = policy.loc.as_ref().ok_or_else(|| {
            AuthzError::PolicyParse("a parsed statement has no source location".into())
        })?;
        let span = loc.start()..loc.end();
        assert!(span.end <= text.len(), "statement span outside its text");
        #[allow(unreachable_patterns)] // an error node exists only with `tolerant-ast`
        let labelled = match &policy.node {
            Some(cst::Policy::Policy(statement)) => statement.annotations.iter().any(|annotation| {
                annotation.node.as_ref().is_some_and(|annotation| {
                    matches!(&annotation.key.node, Some(cst::Ident::Ident(key)) if key == SOURCE_ANNOTATION)
                })
            }),
            _ => false,
        };
        spans.push((span, labelled));
    }
    Ok(spans)
}

/// The identities of every statement in `text`.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn statement_keys(text: &str) -> Result<BTreeSet<StatementKey>, AuthzError> {
    Ok(policy_statements(text)?
        .into_iter()
        .map(|(_, key)| key)
        .collect())
}

/// `text` without the statements whose identity is in `remove`.
///
/// Each removed statement takes the line break that followed it; all other
/// bytes stay as they were.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn remove_statements(
    text: &str,
    remove: &BTreeSet<StatementKey>,
) -> Result<String, AuthzError> {
    if remove.is_empty() {
        return Ok(text.to_string());
    }
    let mut kept = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut removed = false;
    for (span, key) in policy_statements(text)? {
        if !remove.contains(&key) {
            continue;
        }
        kept.push_str(&text[cursor..span.start]);
        cursor = span.end;
        for line_break in ["\r\n", "\n"] {
            if text[cursor..].starts_with(line_break) {
                cursor += line_break.len();
                break;
            }
        }
        removed = true;
    }
    if !removed {
        return Ok(text.to_string());
    }
    kept.push_str(&text[cursor..]);
    debug_assert!(kept.len() < text.len());
    Ok(kept)
}

/// The annotation that names where a statement came from, so a decision can
/// say which rule decided it (see [`label_statements`]).
pub const SOURCE_ANNOTATION: &str = "source";

/// `text` with every statement annotated `@source("{label}#{n}")`, `n` the
/// statement's 1-based position in `text`, so a denial reads
/// `katagami-commons/art_style.cedar#2` instead of a positional `policy1874`.
/// The annotation travels with the text into every store and reload; Cedar
/// keeps it on the parsed policy. A statement that already has a source
/// annotation keeps it. Every other byte stays as it was.
///
/// # Errors
///
/// Returns [`AuthzError::PolicyParse`] when `text` is not valid Cedar.
pub fn label_statements(text: &str, label: &str) -> Result<String, AuthzError> {
    let label = label.replace('\\', "\\\\").replace('"', "\\\"");
    let mut labelled = String::with_capacity(text.len() + 64);
    let mut cursor = 0;
    for (index, (span, already_labelled)) in statement_spans(text)?.into_iter().enumerate() {
        if already_labelled {
            continue;
        }
        labelled.push_str(&text[cursor..span.start]);
        labelled.push_str(&format!(
            "@{SOURCE_ANNOTATION}(\"{label}#{}\")\n",
            index + 1
        ));
        cursor = span.start;
    }
    labelled.push_str(&text[cursor..]);
    Ok(labelled)
}

/// Statement keys by exact statement text. An install reads every statement
/// of the tenant's rows and live text, and a boot installs every app, so the
/// same text comes back many times; the JSON serialisation is the cost.
fn statement_key(source: &str) -> Result<StatementKey, AuthzError> {
    static KEYS: OnceLock<Mutex<HashMap<String, StatementKey>>> = OnceLock::new();
    const MAX_KEYS: usize = 20_000;
    let keys = KEYS.get_or_init(Default::default);
    if let Some(key) = keys.lock().ok().and_then(|keys| keys.get(source).cloned()) {
        return Ok(key);
    }
    let key = parse_statement_key(source)?;
    if let Ok(mut keys) = keys.lock() {
        if keys.len() >= MAX_KEYS {
            keys.clear();
        }
        keys.insert(source.to_string(), key.clone());
    }
    Ok(key)
}

fn parse_statement_key(source: &str) -> Result<StatementKey, AuthzError> {
    let parse_error = |error: &dyn std::fmt::Display| AuthzError::PolicyParse(error.to_string());
    // A statement with slots parses only as a template, one without only as a policy.
    let mut json = match Policy::parse(None, source) {
        Ok(policy) => policy.to_json().map_err(|error| parse_error(&error))?,
        Err(policy_error) => Template::parse(None, source)
            .map_err(|_| parse_error(&policy_error))?
            .to_json()
            .map_err(|error| parse_error(&error))?,
    };
    // The source label says where a statement came from, not what it means:
    // a labelled statement and its unlabelled copy are the same statement.
    let no_annotations_left = json
        .get_mut("annotations")
        .and_then(serde_json::Value::as_object_mut)
        .is_some_and(|annotations| {
            annotations.remove(SOURCE_ANNOTATION);
            annotations.is_empty()
        });
    if let (true, Some(statement)) = (no_annotations_left, json.as_object_mut()) {
        statement.remove("annotations");
    }
    Ok(StatementKey(json.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORBID: &str = "forbid(principal, action == Action::\"Withdraw\", resource is Thing);";
    const PERMIT: &str = "permit(principal, action, resource is Thing);";

    #[test]
    fn labels_each_statement_with_its_source_and_position() {
        let text = format!("// rules\n{PERMIT}\n\n{FORBID}\n");
        let labelled = label_statements(&text, "bank/policies.cedar").unwrap();
        assert_eq!(
            labelled,
            format!(
                "// rules\n@source(\"bank/policies.cedar#1\")\n{PERMIT}\n\n@source(\"bank/policies.cedar#2\")\n{FORBID}\n"
            )
        );
        // Already labelled statements are left as they are.
        assert_eq!(
            label_statements(&labelled, "other.cedar").unwrap(),
            labelled
        );
        let set: cedar_policy::PolicySet = labelled.parse().unwrap();
        let mut sources: Vec<&str> = set
            .policies()
            .filter_map(|policy| policy.annotation(SOURCE_ANNOTATION))
            .collect();
        sources.sort_unstable();
        assert_eq!(sources, ["bank/policies.cedar#1", "bank/policies.cedar#2"]);
    }

    #[test]
    fn same_statement_in_other_layout_has_the_same_key() {
        let reformatted = "// frozen\r\nforbid(\r\n  principal,\r\n  action == Action::\"Withdraw\",\r\n  resource is Thing\r\n);";
        assert_eq!(
            statement_keys(FORBID).unwrap(),
            statement_keys(reformatted).unwrap()
        );
        assert_ne!(
            statement_keys(FORBID).unwrap(),
            statement_keys(PERMIT).unwrap()
        );
    }

    #[test]
    fn a_labelled_statement_has_its_unlabelled_key() {
        let labelled = label_statements(FORBID, "bank/vault.cedar").unwrap();
        assert_ne!(labelled, FORBID);
        assert_eq!(
            statement_keys(&labelled).unwrap(),
            statement_keys(FORBID).unwrap()
        );
        let other = format!("@id(\"guard\")\n{FORBID}");
        assert_ne!(
            statement_keys(&other).unwrap(),
            statement_keys(FORBID).unwrap()
        );
    }

    #[test]
    fn removes_only_the_named_statements() {
        let text = format!(
            "// keep me\n{PERMIT}\nforbid(\r\n  principal,\r\n  action == Action::\"Withdraw\",\r\n  resource is Thing\r\n);\r\n// trailing\n"
        );
        let remove = statement_keys(FORBID).unwrap();
        let stripped = remove_statements(&text, &remove).unwrap();
        assert_eq!(stripped, format!("// keep me\n{PERMIT}\n// trailing\n"));
        assert!(statement_keys(&stripped).unwrap().is_disjoint(&remove));
    }

    #[test]
    fn text_without_the_statement_is_unchanged() {
        let remove = statement_keys(FORBID).unwrap();
        assert_eq!(remove_statements(PERMIT, &remove).unwrap(), PERMIT);
        assert_eq!(remove_statements("", &remove).unwrap(), "");
        assert!(remove_statements("permit(", &remove).is_err());
    }
}
