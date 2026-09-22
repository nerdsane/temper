//! Small typed comparison grammar, shared by execution and verification.

use super::decimal::{SCALE, fixed};
use super::{SystemOneGuard, SystemOneQuestion};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Ne,
    Ge,
    Gt,
    Le,
    Lt,
}

impl Op {
    fn accepts<T: PartialOrd + PartialEq>(&self, left: &T, right: &T) -> bool {
        match self {
            Self::Eq => left == right,
            Self::Ne => left != right,
            Self::Ge => left >= right,
            Self::Gt => left > right,
            Self::Le => left <= right,
            Self::Lt => left < right,
        }
    }
}

enum Scalar {
    Text(String),
    Number(i128),
}

pub(super) struct Clause {
    question: String,
    field: String,
    op: Op,
    value: Scalar,
}

pub(super) fn parse(guard: &SystemOneGuard) -> Result<Vec<Clause>, String> {
    let clauses = conjunctions(&guard.assertion)?;
    clauses
        .iter()
        .map(|clause| parse_clause(guard, clause))
        .collect()
}

fn parse_clause(guard: &SystemOneGuard, clause: &str) -> Result<Clause, String> {
    let (left, op, right) = comparison(clause)?;
    let parts: Vec<_> = left.trim().split('.').collect();
    if parts.len() != 3 || parts[0] != "answers" {
        return Err("system_one assertion expects answers.<question>.<field>".into());
    }
    let question = guard
        .questions
        .get(parts[1])
        .ok_or_else(|| format!("unknown system_one question '{}'", parts[1]))?;
    let field = parts[2];
    let value = match (question, field) {
        (SystemOneQuestion::Choice { criteria, .. }, "choice") => {
            if !matches!(op, Op::Eq | Op::Ne) {
                return Err("choice assertions support only == and !=".into());
            }
            let text: String = serde_json::from_str(right.trim())
                .map_err(|_| "choice assertion requires a JSON quoted string")?;
            if !criteria.contains_key(&text) {
                return Err(format!("unknown choice option '{text}'"));
            }
            Scalar::Text(text)
        }
        (SystemOneQuestion::Noul { .. }, "noul")
        | (SystemOneQuestion::Score { .. }, "score")
        | (SystemOneQuestion::Choice { .. } | SystemOneQuestion::Score { .. }, "confidence") => {
            Scalar::Number(fixed(right.trim())?)
        }
        _ => {
            return Err(format!(
                "unsupported answer field '{field}' for {}",
                question.type_name()
            ));
        }
    };
    Ok(Clause {
        question: parts[1].into(),
        field: field.into(),
        op,
        value,
    })
}

fn comparison(clause: &str) -> Result<(&str, Op, &str), String> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in clause.char_indices() {
        if ch == '"' && !escaped {
            quoted = !quoted;
        }
        if !quoted {
            for (token, op) in [
                ("==", Op::Eq),
                ("!=", Op::Ne),
                (">=", Op::Ge),
                ("<=", Op::Le),
                (">", Op::Gt),
                ("<", Op::Lt),
            ] {
                if clause[index..].starts_with(token) {
                    return Ok((&clause[..index], op, &clause[index + token.len()..]));
                }
            }
        }
        escaped = ch == '\\' && !escaped;
    }
    Err("system_one assertion requires a scalar comparison".into())
}

fn conjunctions(expression: &str) -> Result<Vec<&str>, String> {
    let mut result = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut indices = expression.char_indices().peekable();
    while let Some((index, ch)) = indices.next() {
        if ch == '"' && !escaped {
            quoted = !quoted;
        }
        if !quoted && ch == '&' && indices.peek().is_some_and(|(_, c)| *c == '&') {
            indices.next();
            result.push(expression[start..index].trim());
            start = index + 2;
        }
        escaped = ch == '\\' && !escaped;
    }
    result.push(expression[start..].trim());
    if quoted || result.iter().any(|s| s.is_empty()) {
        return Err("invalid system_one assertion conjunction".into());
    }
    Ok(result)
}

pub(super) fn evaluate(guard: &SystemOneGuard, response: &Value) -> Result<bool, String> {
    for clause in parse(guard)? {
        let actual = &response["answers"][&clause.question][&clause.field];
        let passes = match clause.value {
            Scalar::Text(expected) => clause.op.accepts(
                &actual.as_str().ok_or("expected string answer")?,
                &expected.as_str(),
            ),
            Scalar::Number(expected) => {
                let actual = actual.as_number().ok_or("expected numeric answer")?;
                clause.op.accepts(&fixed(&actual.to_string())?, &expected)
            }
        };
        if !passes {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn may_hold(guard: &SystemOneGuard) -> Result<bool, String> {
    let clauses = parse(guard)?;
    let mut groups: BTreeMap<(&str, &str), Vec<&Clause>> = BTreeMap::new();
    for clause in &clauses {
        groups
            .entry((&clause.question, &clause.field))
            .or_default()
            .push(clause);
    }
    for ((question, field), clauses) in groups {
        if field == "choice" {
            let SystemOneQuestion::Choice { criteria, .. } = &guard.questions[question] else {
                unreachable!()
            };
            let any = criteria.keys().any(|option| {
                clauses.iter().all(|clause| {
                    let Scalar::Text(expected) = &clause.value else {
                        unreachable!()
                    };
                    clause.op.accepts(option, expected)
                })
            });
            if !any {
                return Ok(false);
            }
        } else {
            let mut low = 0;
            let mut high = match (&guard.questions[question], field) {
                (SystemOneQuestion::Score { criteria, .. }, "score") => {
                    (criteria.len() - 1) as i128 * SCALE
                }
                _ => SCALE,
            };
            let mut excluded = BTreeSet::new();
            for clause in clauses {
                let Scalar::Number(number) = clause.value else {
                    unreachable!()
                };
                match clause.op {
                    Op::Eq => {
                        low = low.max(number);
                        high = high.min(number);
                    }
                    Op::Ne => {
                        excluded.insert(number);
                    }
                    Op::Ge => low = low.max(number),
                    Op::Gt => low = low.max(number.saturating_add(1)),
                    Op::Le => high = high.min(number),
                    Op::Lt => high = high.min(number.saturating_sub(1)),
                }
            }
            if low > high
                || high.saturating_sub(low).saturating_add(1)
                    <= excluded.range(low..=high).count() as i128
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
