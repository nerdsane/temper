//! JSON <-> Monty object conversions.

use monty::{DictPairs, MontyObject};
use serde_json::{Map, Value, json};

/// Convert a JSON [`Value`] into a [`MontyObject`].
pub fn json_to_monty_object(value: &Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::None,
        Value::Bool(v) => MontyObject::Bool(*v),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                MontyObject::Int(i)
            } else if let Some(u) = v.as_u64() {
                if u <= i64::MAX as u64 {
                    MontyObject::Int(u as i64)
                } else {
                    MontyObject::String(u.to_string())
                }
            } else {
                MontyObject::Float(v.as_f64().unwrap_or_default())
            }
        }
        Value::String(v) => MontyObject::String(v.clone()),
        Value::Array(items) => MontyObject::List(items.iter().map(json_to_monty_object).collect()),
        Value::Object(map) => {
            let pairs = map
                .iter()
                .map(|(k, v)| (MontyObject::String(k.clone()), json_to_monty_object(v)))
                .collect::<Vec<_>>();
            MontyObject::Dict(DictPairs::from(pairs))
        }
    }
}

/// Convert a [`MontyObject`] into a JSON [`Value`].
pub fn monty_object_to_json(value: &MontyObject) -> Value {
    match value {
        MontyObject::Ellipsis => json!({"$ellipsis": true}),
        MontyObject::None => Value::Null,
        MontyObject::Bool(v) => Value::Bool(*v),
        MontyObject::Int(v) => Value::from(*v),
        MontyObject::BigInt(v) => Value::String(v.to_string()),
        MontyObject::Float(v) => serde_json::Number::from_f64(*v)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        MontyObject::String(v) => Value::String(v.clone()),
        MontyObject::Bytes(bytes) => Value::Array(bytes.iter().copied().map(Value::from).collect()),
        MontyObject::List(items)
        | MontyObject::Tuple(items)
        | MontyObject::Set(items)
        | MontyObject::FrozenSet(items) => {
            Value::Array(items.iter().map(monty_object_to_json).collect())
        }
        MontyObject::NamedTuple {
            field_names,
            values,
            ..
        } => {
            let mut out = Map::new();
            for (field_name, field_value) in field_names.iter().zip(values.iter()) {
                out.insert(field_name.clone(), monty_object_to_json(field_value));
            }
            Value::Object(out)
        }
        MontyObject::Dict(pairs) => {
            let mut out = Map::new();
            for (key, value) in pairs {
                out.insert(monty_key_to_string(key), monty_object_to_json(value));
            }
            Value::Object(out)
        }
        MontyObject::Exception { exc_type, arg } => {
            json!({"$exception": {"type": exc_type.to_string(), "message": arg}})
        }
        MontyObject::Type(t) => Value::String(format!("<class '{}'>", t)),
        MontyObject::BuiltinFunction(name) => Value::String(name.to_string()),
        MontyObject::Path(path) => Value::String(path.clone()),
        MontyObject::Dataclass { name, attrs, .. } => {
            let mut out = Map::new();
            out.insert("$class".to_string(), Value::String(name.clone()));
            for (key, value) in attrs {
                out.insert(monty_key_to_string(key), monty_object_to_json(value));
            }
            Value::Object(out)
        }
        MontyObject::Function { name, .. } => Value::String(format!("<function {name}>")),
        MontyObject::Repr(value) => Value::String(value.clone()),
        MontyObject::Cycle(_, placeholder) => Value::String(placeholder.clone()),
    }
}

fn monty_key_to_string(key: &MontyObject) -> String {
    match key {
        MontyObject::String(s) => s.clone(),
        _ => key.to_string(),
    }
}
