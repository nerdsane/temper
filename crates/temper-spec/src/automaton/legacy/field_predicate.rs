//! The pre-grammar `[[field_invariant]]` predicate tables (`when`/`require`),
//! kept only to convert old specs.
//!
//! A field invariant declares: "when this predicate over the entity's fields
//! matches, this other predicate must also match, or the write is rejected."
//!
//! See `docs/adrs/0041-ioa-field-invariants.md` for the full grammar and
//! motivation.
//!
//! ## Grammar
//!
//! Leaves inspect exactly one field of the `initial_fields` snapshot:
//!
//! - `{ field = X, absent = true }` — passes when `X` is missing or `null`.
//! - `{ field = X, equals = V }`   — passes when `X` exists and equals `V`.
//! - `{ field = X, empty  = true }` — convenience for "absent, null, empty
//!   string, or empty array".
//!
//! Combinators compose predicates:
//!
//! - `{ any_of = [ ... ] }` — OR, short-circuits on first pass
//! - `{ all_of = [ ... ] }` — AND, short-circuits on first fail
//! - `{ not    = { ... } }` — NOT
//!
//! Both `when` and `require` accept the same grammar. A bare predicate is
//! its own base case — no wrapping required.

use std::fmt;

use serde::{Deserialize, Serialize, de};
use serde_json::Value as Json;

/// A predicate tree over a `serde_json::Value` field bag.
///
/// Leaves inspect one field; combinators compose child predicates. Literals
/// on `Equals` are stored as `serde_json::Value` so bools, strings, numbers,
/// and enum member names all flow through the same code path as the OData
/// write payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FieldPredicate {
    /// Field is missing or `null`.
    Absent {
        field: String,
        #[serde(serialize_with = "serialize_true")]
        absent: (),
    },
    /// Field exists and equals the given literal.
    Equals { field: String, equals: Json },
    /// Convenience leaf — absent, null, empty string, or empty array.
    Empty {
        field: String,
        #[serde(serialize_with = "serialize_true")]
        empty: (),
    },
    /// Logical OR — short-circuits on first child pass.
    AnyOf {
        #[serde(rename = "any_of")]
        any_of: Vec<FieldPredicate>,
    },
    /// Logical AND — short-circuits on first child fail.
    AllOf {
        #[serde(rename = "all_of")]
        all_of: Vec<FieldPredicate>,
    },
    /// Logical NOT.
    Not { not: Box<FieldPredicate> },
}

fn serialize_true<S: serde::Serializer>(_: &(), s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bool(true)
}

// Custom deserializer: we need precise error messages and strict rejection of
// mixed operators (e.g. `{ field, absent = true, equals = "x" }`), which the
// default untagged dispatch can silently accept.
impl<'de> Deserialize<'de> for FieldPredicate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let raw = toml::Value::deserialize(deserializer)?;
        FieldPredicate::from_toml_value(&raw).map_err(de::Error::custom)
    }
}

/// Parse error raised while interpreting a predicate node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateParseError(String);

impl fmt::Display for PredicateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PredicateParseError {}

impl FieldPredicate {
    /// Recursive interpretation of a `toml::Value` into a predicate node.
    ///
    /// Rejects:
    /// - Non-tables at the predicate position.
    /// - Tables mixing a leaf key (`field`) with a combinator key (`any_of` etc).
    /// - Tables with more than one combinator key.
    /// - Tables with more than one operator key on the same leaf.
    /// - Tables with an unknown key.
    /// - `absent`/`empty` values that are not `true`.
    /// - Missing operator on a `field = ...` leaf.
    fn from_toml_value(value: &toml::Value) -> Result<Self, PredicateParseError> {
        let table = value
            .as_table()
            .ok_or_else(|| err("predicate must be an inline table"))?;

        let mut has_field = false;
        let mut has_absent = false;
        let mut has_equals = false;
        let mut has_empty = false;
        let mut has_any_of = false;
        let mut has_all_of = false;
        let mut has_not = false;
        for key in table.keys() {
            match key.as_str() {
                "field" => has_field = true,
                "absent" => has_absent = true,
                "equals" => has_equals = true,
                "empty" => has_empty = true,
                "any_of" => has_any_of = true,
                "all_of" => has_all_of = true,
                "not" => has_not = true,
                other => return Err(err(format!("unknown predicate key `{other}`"))),
            }
        }

        let combinator_count =
            usize::from(has_any_of) + usize::from(has_all_of) + usize::from(has_not);
        let operator_count =
            usize::from(has_absent) + usize::from(has_equals) + usize::from(has_empty);

        if combinator_count > 0 && (has_field || operator_count > 0) {
            return Err(err(
                "predicate cannot mix combinator (`any_of`/`all_of`/`not`) with leaf keys (`field`/`absent`/`equals`/`empty`)",
            ));
        }
        if combinator_count > 1 {
            return Err(err(
                "predicate must contain exactly one of `any_of`, `all_of`, or `not`",
            ));
        }

        // Combinator branch.
        if has_any_of {
            let children = table.get("any_of").unwrap(); // ci-ok: key matched in enclosing arm
            let list = children
                .as_array()
                .ok_or_else(|| err("`any_of` must be an array of predicates"))?;
            let mut out = Vec::with_capacity(list.len());
            for (i, child) in list.iter().enumerate() {
                out.push(
                    FieldPredicate::from_toml_value(child)
                        .map_err(|e| err(format!("any_of[{i}]: {e}")))?,
                );
            }
            return Ok(FieldPredicate::AnyOf { any_of: out });
        }
        if has_all_of {
            let children = table.get("all_of").unwrap(); // ci-ok: key matched in enclosing arm
            let list = children
                .as_array()
                .ok_or_else(|| err("`all_of` must be an array of predicates"))?;
            let mut out = Vec::with_capacity(list.len());
            for (i, child) in list.iter().enumerate() {
                out.push(
                    FieldPredicate::from_toml_value(child)
                        .map_err(|e| err(format!("all_of[{i}]: {e}")))?,
                );
            }
            return Ok(FieldPredicate::AllOf { all_of: out });
        }
        if has_not {
            let child_val = table.get("not").unwrap(); // ci-ok: key matched in enclosing arm
            let inner =
                FieldPredicate::from_toml_value(child_val).map_err(|e| err(format!("not: {e}")))?;
            return Ok(FieldPredicate::Not {
                not: Box::new(inner),
            });
        }

        // Leaf branch — must have `field` and exactly one operator.
        if !has_field {
            return Err(err(
                "predicate must be a combinator (`any_of`/`all_of`/`not`) or a leaf with `field = <name>` plus one operator (`absent`/`equals`/`empty`)",
            ));
        }
        if operator_count == 0 {
            return Err(err(
                "leaf predicate must specify exactly one of `absent`, `equals`, or `empty`",
            ));
        }
        if operator_count > 1 {
            return Err(err(
                "leaf predicate must specify exactly one of `absent`, `equals`, or `empty` — not multiple",
            ));
        }

        let field = table
            .get("field")
            .and_then(|v| v.as_str())
            .ok_or_else(|| err("`field` must be a string"))?
            .to_string();
        if field.is_empty() {
            return Err(err("`field` must be a non-empty string"));
        }

        if has_absent {
            let v = table.get("absent").unwrap(); // ci-ok: key matched in enclosing arm
            let b = v
                .as_bool()
                .ok_or_else(|| err("`absent` must be the literal `true`"))?;
            if !b {
                return Err(err(
                    "`absent = false` is not supported; use `{ not = { field, absent = true } }` for \"present\"",
                ));
            }
            return Ok(FieldPredicate::Absent { field, absent: () });
        }
        if has_empty {
            let v = table.get("empty").unwrap(); // ci-ok: key matched in enclosing arm
            let b = v
                .as_bool()
                .ok_or_else(|| err("`empty` must be the literal `true`"))?;
            if !b {
                return Err(err(
                    "`empty = false` is not supported; use `{ not = { field, empty = true } }` for \"non-empty\"",
                ));
            }
            return Ok(FieldPredicate::Empty { field, empty: () });
        }

        // has_equals
        let v = table.get("equals").unwrap(); // ci-ok: key matched in enclosing arm
        let equals = toml_literal_to_json(v)
            .map_err(|e| err(format!("`equals` value on field `{field}`: {e}")))?;
        Ok(FieldPredicate::Equals { field, equals })
    }

    /// Evaluate this predicate against an entity's `initial_fields` snapshot.
    ///
    /// `fields` is expected to be a JSON object. Non-object inputs produce a
    /// conservative `false` for leaves (no field can match a non-object) and
    /// propagate through combinators normally.
    #[cfg(test)]
    pub fn evaluate(&self, fields: &Json) -> bool {
        match self {
            FieldPredicate::Absent { field, .. } => is_absent_or_null(fields, field),
            FieldPredicate::Equals { field, equals } => equals_value(fields, field, equals),
            FieldPredicate::Empty { field, .. } => is_empty(fields, field),
            FieldPredicate::AnyOf { any_of } => any_of.iter().any(|p| p.evaluate(fields)),
            FieldPredicate::AllOf { all_of } => all_of.iter().all(|p| p.evaluate(fields)),
            FieldPredicate::Not { not } => !not.evaluate(fields),
        }
    }
}

fn err(msg: impl Into<String>) -> PredicateParseError {
    PredicateParseError(msg.into())
}

fn toml_literal_to_json(v: &toml::Value) -> Result<Json, String> {
    match v {
        toml::Value::String(s) => Ok(Json::String(s.clone())),
        toml::Value::Integer(i) => Ok(Json::Number((*i).into())),
        toml::Value::Boolean(b) => Ok(Json::Bool(*b)),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .ok_or_else(|| "float literal is not finite".to_string()),
        toml::Value::Datetime(_) => {
            Err("datetime literals are not supported in equals comparisons".into())
        }
        toml::Value::Array(_) => {
            Err("array literals are not supported in equals comparisons".into())
        }
        toml::Value::Table(_) => {
            Err("table literals are not supported in equals comparisons".into())
        }
    }
}

#[cfg(test)]
fn field_value<'a>(fields: &'a Json, field: &str) -> Option<&'a Json> {
    fields.as_object().and_then(|obj| obj.get(field))
}

#[cfg(test)]
fn is_absent_or_null(fields: &Json, field: &str) -> bool {
    match field_value(fields, field) {
        None => true,
        Some(Json::Null) => true,
        Some(_) => false,
    }
}

#[cfg(test)]
fn equals_value(fields: &Json, field: &str, expected: &Json) -> bool {
    match field_value(fields, field) {
        Some(actual) if !actual.is_null() => actual == expected,
        _ => false,
    }
}

#[cfg(test)]
fn is_empty(fields: &Json, field: &str) -> bool {
    match field_value(fields, field) {
        None => true,
        Some(Json::Null) => true,
        Some(Json::String(s)) => s.is_empty(),
        Some(Json::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn predicate(toml_src: &str) -> FieldPredicate {
        #[derive(Deserialize)]
        struct W {
            p: FieldPredicate,
        }
        let wrapped = format!("p = {toml_src}");
        toml::from_str::<W>(&wrapped)
            .unwrap_or_else(|e| panic!("failed to parse predicate {toml_src:?}: {e}"))
            .p
    }

    fn predicate_err(toml_src: &str) -> String {
        #[derive(Deserialize)]
        struct W {
            #[allow(dead_code)]
            p: FieldPredicate,
        }
        let wrapped = format!("p = {toml_src}");
        toml::from_str::<W>(&wrapped)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| panic!("expected parse error for {toml_src:?}"))
    }

    #[test]
    fn absent_leaf_matches_missing_or_null() {
        let p = predicate(r#"{ field = "X", absent = true }"#);
        assert!(p.evaluate(&json!({})));
        assert!(p.evaluate(&json!({ "X": null })));
        assert!(!p.evaluate(&json!({ "X": "v" })));
        assert!(!p.evaluate(&json!({ "X": false })));
    }

    #[test]
    fn absent_false_is_rejected() {
        let msg = predicate_err(r#"{ field = "X", absent = false }"#);
        assert!(msg.contains("absent = false"), "error was: {msg}");
    }

    #[test]
    fn equals_leaf_matches_string() {
        let p = predicate(r#"{ field = "ConfigType", equals = "Local" }"#);
        assert!(p.evaluate(&json!({ "ConfigType": "Local" })));
        assert!(!p.evaluate(&json!({ "ConfigType": "Cloud" })));
        assert!(!p.evaluate(&json!({})));
        assert!(!p.evaluate(&json!({ "ConfigType": null })));
    }

    #[test]
    fn equals_leaf_matches_bool() {
        let p = predicate(r#"{ field = "Allowed", equals = true }"#);
        assert!(p.evaluate(&json!({ "Allowed": true })));
        assert!(!p.evaluate(&json!({ "Allowed": false })));
        assert!(!p.evaluate(&json!({ "Allowed": "true" })));
    }

    #[test]
    fn equals_leaf_matches_integer() {
        let p = predicate(r#"{ field = "Count", equals = 3 }"#);
        assert!(p.evaluate(&json!({ "Count": 3 })));
        assert!(!p.evaluate(&json!({ "Count": 4 })));
    }

    #[test]
    fn empty_leaf_matches_missing_null_empty_string_empty_array() {
        let p = predicate(r#"{ field = "Hosts", empty = true }"#);
        assert!(p.evaluate(&json!({})));
        assert!(p.evaluate(&json!({ "Hosts": null })));
        assert!(p.evaluate(&json!({ "Hosts": "" })));
        assert!(p.evaluate(&json!({ "Hosts": [] })));
        assert!(!p.evaluate(&json!({ "Hosts": ["a"] })));
        assert!(!p.evaluate(&json!({ "Hosts": "a" })));
    }

    #[test]
    fn any_of_short_circuits_on_first_pass() {
        let p = predicate(
            r#"{ any_of = [
              { field = "A", absent = true },
              { field = "B", equals = false },
            ]}"#,
        );
        assert!(p.evaluate(&json!({ "B": false })));
        assert!(p.evaluate(&json!({})));
        assert!(!p.evaluate(&json!({ "A": 1, "B": true })));
    }

    #[test]
    fn all_of_short_circuits_on_first_fail() {
        let p = predicate(
            r#"{ all_of = [
              { field = "A", equals = "x" },
              { field = "B", equals = "y" },
            ]}"#,
        );
        assert!(p.evaluate(&json!({ "A": "x", "B": "y" })));
        assert!(!p.evaluate(&json!({ "A": "x", "B": "z" })));
        assert!(!p.evaluate(&json!({ "A": "q", "B": "y" })));
    }

    #[test]
    fn not_inverts_child() {
        let p = predicate(r#"{ not = { field = "A", equals = "x" } }"#);
        assert!(p.evaluate(&json!({ "A": "y" })));
        assert!(!p.evaluate(&json!({ "A": "x" })));
        assert!(p.evaluate(&json!({})));
    }

    #[test]
    fn nested_combinators_work() {
        let p = predicate(
            r#"{ all_of = [
              { field = "Type", equals = "Local" },
              { any_of = [
                { field = "Flag", absent = true },
                { field = "Flag", equals = false },
              ]},
            ]}"#,
        );
        assert!(p.evaluate(&json!({ "Type": "Local" })));
        assert!(p.evaluate(&json!({ "Type": "Local", "Flag": false })));
        assert!(!p.evaluate(&json!({ "Type": "Local", "Flag": true })));
        assert!(!p.evaluate(&json!({ "Type": "Cloud" })));
    }

    #[test]
    fn evaluate_on_non_object_returns_false_for_leaves() {
        let p = predicate(r#"{ field = "X", equals = "v" }"#);
        assert!(!p.evaluate(&json!(null)));
        assert!(!p.evaluate(&json!(42)));
        assert!(!p.evaluate(&json!("scalar")));
    }

    #[test]
    fn mixed_leaf_and_combinator_is_rejected() {
        let msg = predicate_err(r#"{ field = "X", absent = true, any_of = [] }"#);
        assert!(msg.contains("mix combinator"), "error was: {msg}");
    }

    #[test]
    fn mixed_operators_on_leaf_are_rejected() {
        let msg = predicate_err(r#"{ field = "X", absent = true, equals = "y" }"#);
        assert!(msg.contains("exactly one"), "error was: {msg}");
    }

    #[test]
    fn leaf_without_operator_is_rejected() {
        let msg = predicate_err(r#"{ field = "X" }"#);
        assert!(msg.contains("exactly one"), "error was: {msg}");
    }

    #[test]
    fn unknown_predicate_key_is_rejected() {
        let msg = predicate_err(r#"{ field = "X", wibble = true }"#);
        assert!(msg.contains("unknown predicate key"), "error was: {msg}");
    }

    #[test]
    fn multiple_combinators_rejected() {
        let msg = predicate_err(r#"{ any_of = [], all_of = [] }"#);
        assert!(msg.contains("exactly one"), "error was: {msg}");
    }
}
