//! Explicit, bounded construction of external inference context.

use std::io::{self, Write};

use serde_json::Value;

const STATE_BYTE_BUDGET: usize = 64 * 1024;

pub(super) fn validate(template: &Value) -> Result<(), String> {
    if !super::context_type(template) {
        return Err("system_one state must resolve from text, object, or array".into());
    }
    visit(template, None, None, 0, &mut 0, &mut None).map(|_| ())
}

pub(super) fn resolve(template: &Value, entity: &Value, params: &Value) -> Result<Value, String> {
    let mut budget = Some(StateByteBudget(STATE_BYTE_BUDGET));
    let value = visit(template, Some(entity), Some(params), 0, &mut 0, &mut budget)?;
    if !super::context_type(&value) {
        return Err("resolved system_one state must be text, object, or array".into());
    }
    Ok(value)
}

fn visit(
    value: &Value,
    entity: Option<&Value>,
    params: Option<&Value>,
    depth: usize,
    nodes: &mut usize,
    budget: &mut Option<StateByteBudget>,
) -> Result<Value, String> {
    *nodes += 1;
    if depth > 32 || *nodes > 16_384 {
        return Err("system_one state exceeds nesting or node budget".into());
    }
    match value {
        Value::Object(object) if object.contains_key("ref") => {
            if object.len() != 1 {
                return Err("system_one ref binding cannot contain other fields".into());
            }
            let reference = object["ref"]
                .as_str()
                .ok_or("system_one ref must be a string")?;
            let (source, field) = reference
                .split_once('.')
                .ok_or("expected entity.Field or params.Param reference")?;
            if !super::identifier(field) || !matches!(source, "entity" | "params") {
                return Err(format!(
                    "unsupported system_one state reference '{reference}'"
                ));
            }
            let snapshot = if source == "entity" { entity } else { params };
            match snapshot {
                None => Ok(value.clone()),
                Some(snapshot) => {
                    let selected = snapshot.get(field).ok_or_else(|| {
                        format!("missing system_one state reference '{reference}'")
                    })?;
                    charge_value(budget, selected)?;
                    Ok(selected.clone())
                }
            }
        }
        Value::Object(object) => {
            charge_bytes(budget, 2 + object.len().saturating_sub(1))?;
            let resolved = object
                .iter()
                .map(|(k, v)| {
                    if let Some(budget) = budget.as_mut() {
                        serde_json::to_writer(&mut *budget, k).map_err(|_| byte_budget_error())?;
                        budget.charge(1).map_err(|_| byte_budget_error())?;
                    }
                    Ok((
                        k.clone(),
                        visit(v, entity, params, depth + 1, nodes, budget)?,
                    ))
                })
                .collect::<Result<serde_json::Map<_, _>, String>>()?;
            Ok(Value::Object(resolved))
        }
        Value::Array(array) => {
            charge_bytes(budget, 2 + array.len().saturating_sub(1))?;
            array
                .iter()
                .map(|v| visit(v, entity, params, depth + 1, nodes, budget))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        _ => {
            charge_value(budget, value)?;
            Ok(value.clone())
        }
    }
}

fn byte_budget_error() -> String {
    "system_one resolved state byte budget exceeded".into()
}

fn charge_bytes(budget: &mut Option<StateByteBudget>, bytes: usize) -> Result<(), String> {
    if let Some(budget) = budget {
        budget.charge(bytes).map_err(|_| byte_budget_error())?;
    }
    Ok(())
}

fn charge_value(budget: &mut Option<StateByteBudget>, value: &Value) -> Result<(), String> {
    if let Some(budget) = budget {
        // Count escaped JSON bytes before cloning selected data, without allocating a body.
        serde_json::to_writer(budget, value).map_err(|_| byte_budget_error())?;
    }
    Ok(())
}

struct StateByteBudget(usize);

impl StateByteBudget {
    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        self.0 = self
            .0
            .checked_sub(bytes)
            .ok_or_else(|| io::Error::other("state byte budget exceeded"))?;
        Ok(())
    }
}

impl Write for StateByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.charge(bytes.len())?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn references(template: &Value) -> Vec<(String, String)> {
    let mut references = Vec::new();
    let mut pending = vec![template];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) if object.contains_key("ref") => {
                // Callers validate the binding tree before collecting names.
                let reference = object["ref"].as_str().expect("validated reference string");
                let (source, field) = reference
                    .split_once('.')
                    .expect("validated reference source");
                references.push((source.to_string(), field.to_string()));
            }
            Value::Object(object) => pending.extend(object.values()),
            Value::Array(array) => pending.extend(array),
            _ => {}
        }
    }
    references
}
