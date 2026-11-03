//! State variables, their types and starting values, and action parameters.

use serde::{Deserialize, Serialize};

/// The type of a state variable or an action parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    /// Text.
    #[default]
    String,
    /// `true` or `false`.
    Bool,
    /// A non-negative integer, modeled (bounded) by the verifier.
    Counter,
    /// A signed 64-bit integer, not modeled by the verifier.
    Int,
    /// A list of strings.
    List,
}

impl VarType {
    /// How the predicate checker and the verifier read this type.
    pub fn kind(self) -> crate::predicate::VarKind {
        use crate::predicate::VarKind;
        match self {
            VarType::String => VarKind::Str,
            VarType::Bool => VarKind::Bool,
            VarType::Counter => VarKind::Counter,
            VarType::Int => VarKind::Num,
            VarType::List => VarKind::List,
        }
    }

    /// Spec spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            VarType::String => "string",
            VarType::Bool => "bool",
            VarType::Counter => "counter",
            VarType::Int => "int",
            VarType::List => "list",
        }
    }
}

impl std::fmt::Display for VarType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A state variable's starting value, in its declared type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Initial {
    /// A `string` variable.
    String(String),
    /// A `bool` variable.
    Bool(bool),
    /// A `counter` variable.
    Counter(usize),
    /// An `int` variable.
    Int(i64),
    /// A `list` variable.
    List(Vec<String>),
}

impl Initial {
    /// The type this value belongs to.
    pub fn var_type(&self) -> VarType {
        match self {
            Initial::String(_) => VarType::String,
            Initial::Bool(_) => VarType::Bool,
            Initial::Counter(_) => VarType::Counter,
            Initial::Int(_) => VarType::Int,
            Initial::List(_) => VarType::List,
        }
    }

    /// The value as JSON, as it appears in entity fields.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Initial::String(s) => serde_json::Value::String(s.clone()),
            Initial::Bool(b) => serde_json::Value::Bool(*b),
            Initial::Counter(n) => serde_json::Value::from(*n),
            Initial::Int(n) => serde_json::Value::from(*n),
            Initial::List(items) => serde_json::Value::from(items.clone()),
        }
    }

    /// Read `value` as the starting value of a `var_type` variable.
    fn read(var_type: VarType, value: serde_json::Value) -> Result<Self, String> {
        let read = match (var_type, &value) {
            (VarType::String, serde_json::Value::String(s)) => Some(Initial::String(s.clone())),
            (VarType::Bool, serde_json::Value::Bool(b)) => Some(Initial::Bool(*b)),
            (VarType::Counter, v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .map(Initial::Counter),
            (VarType::Int, v) => v.as_i64().map(Initial::Int),
            (VarType::List, serde_json::Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .map(Initial::List),
            _ => None,
        };
        read.ok_or_else(|| {
            let expected = match var_type {
                VarType::String => "a string, such as `initial = \"\"`",
                VarType::Bool => "`true` or `false`",
                VarType::Counter => "a non-negative integer, such as `initial = 0`",
                VarType::Int => "an integer, such as `initial = 0`",
                VarType::List => "an array of strings, such as `initial = []`",
            };
            format!("initial: a {var_type} starts at {expected}, found {value}")
        })
    }
}

impl std::fmt::Display for Initial {
    /// Spec spelling of the value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

/// A state variable declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawStateVar")]
pub struct StateVar {
    /// Variable name.
    pub name: String,
    /// The variable's type.
    #[serde(rename = "type")]
    pub var_type: VarType,
    /// The value a new entity starts with; always of `var_type`.
    pub initial: Initial,
    /// Optional per-field inline ceiling in bytes for the field-overflow
    /// primitive (ADR-0045). Values above this size are moved to the blob
    /// store; values at or below stay inline in `fields`. When `None`, the
    /// crate-wide `DEFAULT_FIELD_INLINE_MAX` applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_inline_max_bytes: Option<usize>,
    /// Optional per-field TTL in seconds for overflow blobs (ADR-0047). When
    /// `None`, overflow blobs are permanent (match pre-ADR behavior). When
    /// set, the blob's `expires_at` is written as `datetime('now', '+N s')`
    /// and the sweeper deletes rows past their expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_ttl_seconds: Option<u64>,
    /// Optional query-plane override. When `Some(false)`, the field remains in
    /// entity state but is omitted from the durable field index used for OData
    /// collection filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_indexed: Option<bool>,
}

/// `[[state]]` as written; `initial` is checked against `type`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStateVar {
    name: String,
    #[serde(rename = "type")]
    var_type: VarType,
    initial: serde_json::Value,
    #[serde(default)]
    overflow_inline_max_bytes: Option<usize>,
    #[serde(default)]
    overflow_ttl_seconds: Option<u64>,
    #[serde(default)]
    query_indexed: Option<bool>,
}

impl TryFrom<RawStateVar> for StateVar {
    type Error = String;

    fn try_from(raw: RawStateVar) -> Result<Self, String> {
        let initial = Initial::read(raw.var_type, raw.initial)?;
        debug_assert_eq!(initial.var_type(), raw.var_type);
        Ok(StateVar {
            name: raw.name,
            var_type: raw.var_type,
            initial,
            overflow_inline_max_bytes: raw.overflow_inline_max_bytes,
            overflow_ttl_seconds: raw.overflow_ttl_seconds,
            query_indexed: raw.query_indexed,
        })
    }
}

/// A parameter on an action: a bare name (a `string`) or a typed declaration.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ActionParam {
    /// `"name"`: a string parameter.
    Named(String),
    /// `{ name = "n", type = "counter" }`.
    Typed {
        /// Parameter name.
        name: String,
        /// Parameter type.
        #[serde(rename = "type")]
        param_type: VarType,
        /// Trusted runtime source. A caller cannot supply this parameter.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<ParameterSource>,
    },
}

/// Authenticated runtime values available to declared action parameters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterSource {
    /// Verified delegated subject when present, otherwise the principal ID.
    AuthenticatedSubject,
    /// The name its issuer gave the calling agent instance, so a record can
    /// keep who made it. Empty for a person in person and an agent with none.
    AuthenticatedAgentLabel,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedParam {
    name: String,
    #[serde(rename = "type", default)]
    param_type: VarType,
    #[serde(default)]
    source: Option<ParameterSource>,
}

impl<'de> Deserialize<'de> for ActionParam {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(name) => Ok(ActionParam::Named(name)),
            table @ serde_json::Value::Object(_) => {
                let TypedParam {
                    name,
                    param_type,
                    source,
                } = serde_json::from_value(table).map_err(D::Error::custom)?;
                Ok(ActionParam::Typed {
                    name,
                    param_type,
                    source,
                })
            }
            other => Err(D::Error::custom(format!(
                "a parameter is a name or {{ name = ..., type = ... }}, found {other}"
            ))),
        }
    }
}

impl ActionParam {
    /// Parameter name.
    pub fn name(&self) -> &str {
        match self {
            Self::Named(n) => n,
            Self::Typed { name, .. } => name,
        }
    }

    /// Runtime source for this parameter, if it is not client supplied.
    pub fn source(&self) -> Option<ParameterSource> {
        match self {
            Self::Named(_) => None,
            Self::Typed { source, .. } => *source,
        }
    }

    /// Parameter type; a bare name is a `string`.
    pub fn param_type(&self) -> VarType {
        match self {
            Self::Named(_) => VarType::String,
            Self::Typed { param_type, .. } => *param_type,
        }
    }
}
