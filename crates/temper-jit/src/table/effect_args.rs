//! Resolve effect [`Arg`]s against an entity's state and the action's params.
//!
//! Shared by every runtime that applies [`super::Effect`]s, so a literal, a
//! variable and `params.p` mean the same thing everywhere. A value that is
//! missing or of the wrong type resolves to `None`; the caller decides
//! whether that skips the effect or counts as zero. A declared variable the
//! record does not store reads as its declared initial value.

use std::collections::BTreeMap;

use temper_spec::predicate::{Arg, Literal};

use super::action_contract::InitialValues;

/// A non-negative integer: a literal, a counter, or a param holding a number
/// or a numeric string.
pub fn count(
    arg: &Arg,
    counters: &BTreeMap<String, usize>,
    declared: &InitialValues,
    params: &serde_json::Value,
) -> Option<usize> {
    match arg {
        Arg::Lit(Literal::Int(n)) => usize::try_from(*n).ok(),
        Arg::Var(name) => Some(declared.counter(counters, name).unwrap_or(0)),
        Arg::Param(name) => match params.get(name)? {
            serde_json::Value::Number(number) => {
                number.as_u64().and_then(|n| usize::try_from(n).ok())
            }
            serde_json::Value::String(text) => text.parse().ok(),
            _ => None,
        },
        Arg::Lit(_) => None,
    }
}

/// A boolean: a literal, a bool variable, or a param holding a boolean or
/// `"true"` / `"false"`.
pub fn boolean(
    arg: &Arg,
    booleans: &BTreeMap<String, bool>,
    declared: &InitialValues,
    params: &serde_json::Value,
) -> Option<bool> {
    match arg {
        Arg::Lit(Literal::Bool(value)) => Some(*value),
        Arg::Var(name) => Some(declared.boolean(booleans, name).unwrap_or(false)),
        Arg::Param(name) => match params.get(name)? {
            serde_json::Value::Bool(value) => Some(*value),
            serde_json::Value::String(text) => text.parse().ok(),
            _ => None,
        },
        Arg::Lit(_) => None,
    }
}

/// A string: a literal, or a param holding a string.
pub fn string(arg: &Arg, params: &serde_json::Value) -> Option<String> {
    match arg {
        Arg::Lit(Literal::Str(value)) => Some(value.clone()),
        Arg::Param(name) => params.get(name)?.as_str().map(str::to_string),
        Arg::Lit(_) | Arg::Var(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::predicate::parse_arg;

    fn arg(source: &str) -> Arg {
        parse_arg(source).unwrap()
    }

    #[test]
    fn resolves_literals_variables_and_params() {
        let counters = BTreeMap::from([("n".to_string(), 4)]);
        let booleans = BTreeMap::from([("ok".to_string(), true)]);
        let params = serde_json::json!({ "size": 7, "text": "12", "flag": "true", "tag": "vip" });
        let none = InitialValues::default();
        assert_eq!(count(&arg("3"), &counters, &none, &params), Some(3));
        assert_eq!(count(&arg("n"), &counters, &none, &params), Some(4));
        assert_eq!(count(&arg("missing"), &counters, &none, &params), Some(0));
        assert_eq!(
            count(&arg("params.size"), &counters, &none, &params),
            Some(7)
        );
        assert_eq!(
            count(&arg("params.text"), &counters, &none, &params),
            Some(12)
        );
        assert_eq!(count(&arg("params.tag"), &counters, &none, &params), None);
        assert_eq!(
            count(&arg("params.absent"), &counters, &none, &params),
            None
        );
        assert_eq!(boolean(&arg("ok"), &booleans, &none, &params), Some(true));
        assert_eq!(
            boolean(&arg("params.flag"), &booleans, &none, &params),
            Some(true)
        );
        assert_eq!(
            boolean(&arg("params.size"), &booleans, &none, &params),
            None
        );
        assert_eq!(string(&arg("'a'"), &params).as_deref(), Some("a"));
        assert_eq!(string(&arg("params.tag"), &params).as_deref(), Some("vip"));
        assert_eq!(string(&arg("params.size"), &params), None);
    }

    #[test]
    fn a_declared_variable_the_record_lacks_reads_as_its_initial_value() {
        let declared = InitialValues {
            counters: BTreeMap::from([("credits".to_string(), 3)]),
            booleans: BTreeMap::from([("open".to_string(), true)]),
            ..InitialValues::default()
        };
        let params = serde_json::json!({});
        let none = BTreeMap::new();
        assert_eq!(count(&arg("credits"), &none, &declared, &params), Some(3));
        assert_eq!(
            boolean(&arg("open"), &BTreeMap::new(), &declared, &params),
            Some(true)
        );
        let stored = BTreeMap::from([("credits".to_string(), 1)]);
        assert_eq!(count(&arg("credits"), &stored, &declared, &params), Some(1));
        assert_eq!(
            count(&arg("undeclared"), &none, &declared, &params),
            Some(0)
        );
    }
}
