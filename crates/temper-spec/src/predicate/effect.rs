//! Effect statements: what an action writes to its entity's state.
//!
//! ```text
//! effect  = assign | call ;
//! assign  = ident ( "=" | "+=" | "-=" ) arg ;
//! call    = "append" "(" ident "," arg ")"
//!         | "remove_at" "(" ident "," arg ")"
//!         | "schedule" "(" 'action' "," int ")"
//!         | "schedule_at" "(" 'action' "," ident ")"
//!         | "spawn" "(" 'type' "," 'action' [ "," ident [ "," arg ] ] ")" ;
//! arg     = int | 'string' | "true" | "false" | ident | "params" "." ident ;
//! ```
//!
//! Literals and names are the predicate language's; `params.p` (the action
//! parameter `p`) is the one addition. Effects serialize as their canonical
//! source text.

use std::collections::BTreeMap;
use std::fmt;

use super::ast::Literal;
use super::check::VarKind;
use super::parse::{ParseError, Parser, Tok};

/// The value an effect writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    /// A literal (never `null`).
    Lit(Literal),
    /// Another state variable of the same type.
    Var(String),
    /// `params.p`: the action parameter `p`.
    Param(String),
}

/// The operator of an assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`.
    Set,
    /// `+=`.
    Add,
    /// `-=` (stops at 0).
    Sub,
}

impl AssignOp {
    /// Source spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            AssignOp::Set => "=",
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
        }
    }
}

/// One effect statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `var = v`, `var += v` or `var -= v`.
    Assign {
        /// The counter or bool written.
        var: String,
        /// The operator.
        op: AssignOp,
        /// The value.
        value: Arg,
    },
    /// `append(list, v)`.
    Append {
        /// The list written.
        list: String,
        /// The string appended.
        value: Arg,
    },
    /// `remove_at(list, i)`: remove the element at index `i`, if present.
    RemoveAt {
        /// The list written.
        list: String,
        /// The index removed.
        index: Arg,
    },
    /// `schedule('Action', seconds)`: dispatch an action on this entity later.
    Schedule {
        /// The action dispatched.
        action: String,
        /// Delay before dispatch.
        delay_seconds: u64,
    },
    /// `schedule_at('Action', field)`: dispatch an action at the time in `field`.
    ScheduleAt {
        /// The action dispatched.
        action: String,
        /// The entity field holding the timestamp.
        field: String,
    },
    /// `spawn('Type', 'Action', field, id)`: create a child entity, dispatch
    /// `Action` on it, and store its id in `field`. The id is `id` (a
    /// `'string'` or `params.p`) when given and present, otherwise fresh.
    Spawn {
        /// The child entity type.
        entity_type: String,
        /// The action dispatched on the child.
        initial_action: String,
        /// The field on this entity that receives the child's id.
        store_id_in: Option<String>,
        /// The child's id, when the caller chooses it.
        id: Option<Arg>,
    },
}

/// Parse one effect statement.
pub fn parse_effect(source: &str) -> Result<Effect, ParseError> {
    let mut parser = Parser::new(source, "effect")?;
    let effect = parser.effect()?;
    if parser.pos < parser.tokens.len() {
        return Err(parser.error("unexpected token after effect"));
    }
    Ok(effect)
}

impl Parser<'_> {
    fn effect(&mut self) -> Result<Effect, ParseError> {
        let name = self.ident()?;
        if let Some(Tok::Assign(op)) = self.peek().cloned() {
            self.pos += 1;
            let value = self.arg()?;
            return Ok(Effect::Assign {
                var: name,
                op,
                value,
            });
        }
        if !self.eat(&Tok::LParen) {
            return Err(self.error("expected '=', '+=', '-=' or '('"));
        }
        let effect = match name.as_str() {
            "append" => {
                let list = self.ident()?;
                self.expect(&Tok::Comma, "','")?;
                Effect::Append {
                    list,
                    value: self.arg()?,
                }
            }
            "remove_at" => {
                let list = self.ident()?;
                self.expect(&Tok::Comma, "','")?;
                Effect::RemoveAt {
                    list,
                    index: self.arg()?,
                }
            }
            "schedule" => {
                let action = self.string()?;
                self.expect(&Tok::Comma, "','")?;
                let delay_seconds = match self.peek().cloned() {
                    Some(Tok::Int(value)) if value >= 0 => {
                        self.pos += 1;
                        value as u64
                    }
                    _ => return Err(self.error("expected a delay in seconds")),
                };
                Effect::Schedule {
                    action,
                    delay_seconds,
                }
            }
            "schedule_at" => {
                let action = self.string()?;
                self.expect(&Tok::Comma, "','")?;
                Effect::ScheduleAt {
                    action,
                    field: self.ident()?,
                }
            }
            "spawn" => {
                let entity_type = self.string()?;
                self.expect(&Tok::Comma, "','")?;
                let initial_action = self.string()?;
                let store_id_in = if self.eat(&Tok::Comma) {
                    Some(self.ident()?)
                } else {
                    None
                };
                let id = if store_id_in.is_some() && self.eat(&Tok::Comma) {
                    Some(self.arg()?)
                } else {
                    None
                };
                Effect::Spawn {
                    entity_type,
                    initial_action,
                    store_id_in,
                    id,
                }
            }
            "emit" | "trigger" => {
                self.pos -= 1;
                return Err(self.error(&format!(
                    "unknown effect '{name}'; declare outgoing calls as [[action.triggers]]"
                )));
            }
            _ => {
                self.pos -= 1;
                return Err(self.error(&format!("unknown effect '{name}'")));
            }
        };
        self.expect(&Tok::RParen, "')'")?;
        Ok(effect)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        match self.peek().cloned() {
            Some(Tok::Str(value)) if !value.is_empty() => {
                self.pos += 1;
                Ok(value)
            }
            _ => Err(self.error("expected a quoted name")),
        }
    }

    fn arg(&mut self) -> Result<Arg, ParseError> {
        let arg = match self.peek().cloned() {
            Some(Tok::Int(value)) => Arg::Lit(Literal::Int(value)),
            Some(Tok::Str(value)) => Arg::Lit(Literal::Str(value)),
            Some(Tok::Ident(name)) if name == "true" || name == "false" => {
                Arg::Lit(Literal::Bool(name == "true"))
            }
            Some(Tok::Ident(name)) if name == "params" => {
                self.pos += 1;
                self.expect(&Tok::Dot, "'.' after 'params'")?;
                return Ok(Arg::Param(self.ident()?));
            }
            Some(Tok::Ident(_)) => return Ok(Arg::Var(self.ident()?)),
            _ => return Err(self.error("expected a value, a name or params.<name>")),
        };
        self.pos += 1;
        Ok(arg)
    }
}

impl fmt::Display for Arg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::Lit(lit) => write!(f, "{lit}"),
            Arg::Var(name) => f.write_str(name),
            Arg::Param(name) => write!(f, "params.{name}"),
        }
    }
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Assign { var, op, value } => write!(f, "{var} {} {value}", op.as_str()),
            Effect::Append { list, value } => write!(f, "append({list}, {value})"),
            Effect::RemoveAt { list, index } => write!(f, "remove_at({list}, {index})"),
            Effect::Schedule {
                action,
                delay_seconds,
            } => write!(f, "schedule('{action}', {delay_seconds})"),
            Effect::ScheduleAt { action, field } => write!(f, "schedule_at('{action}', {field})"),
            Effect::Spawn {
                entity_type,
                initial_action,
                store_id_in,
                id,
            } => {
                write!(f, "spawn('{entity_type}', '{initial_action}'")?;
                if let Some(field) = store_id_in {
                    write!(f, ", {field}")?;
                }
                if let Some(id) = id {
                    write!(f, ", {id}")?;
                }
                f.write_str(")")
            }
        }
    }
}

/// Parse a lone effect argument (`3`, `'a'`, `true`, `x`, `params.p`).
pub fn parse_arg(source: &str) -> Result<Arg, ParseError> {
    let mut parser = Parser::new(source, "effect argument")?;
    let arg = parser.arg()?;
    if parser.pos < parser.tokens.len() {
        return Err(parser.error("unexpected token after value"));
    }
    Ok(arg)
}

impl serde::Serialize for Arg {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Arg {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        parse_arg(&source).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for Effect {
    type Err = ParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        parse_effect(source)
    }
}

impl serde::Serialize for Effect {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Effect {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        parse_effect(&source).map_err(serde::de::Error::custom)
    }
}

/// What an action parameter is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// A non-negative integer (counter values, list indexes).
    Count,
    /// A boolean.
    Bool,
    /// A string (list elements).
    Str,
}

/// Check one effect against the declared state variables. Returns the kind
/// of the action parameter it reads, if any.
pub fn check_effect(
    effect: &Effect,
    vars: &BTreeMap<String, VarKind>,
) -> Result<Option<(String, ParamKind)>, String> {
    let declared = |name: &str| {
        vars.get(name)
            .copied()
            .ok_or_else(|| format!("unknown state variable '{name}'"))
    };
    match effect {
        Effect::Assign { var, op, value } => match declared(var)? {
            VarKind::Counter => match value {
                Arg::Lit(Literal::Int(n)) if *n >= 0 => Ok(None),
                Arg::Var(name) if declared(name)? == VarKind::Counter => Ok(None),
                Arg::Param(p) => Ok(Some((p.clone(), ParamKind::Count))),
                _ => Err(format!(
                    "'{effect}': a counter takes a non-negative integer, a counter or params.<name>"
                )),
            },
            VarKind::Bool => {
                if *op != AssignOp::Set {
                    return Err(format!("'{effect}': '{}' needs a counter", op.as_str()));
                }
                match value {
                    Arg::Lit(Literal::Bool(_)) => Ok(None),
                    Arg::Var(name) if declared(name)? == VarKind::Bool => Ok(None),
                    Arg::Param(p) => Ok(Some((p.clone(), ParamKind::Bool))),
                    _ => Err(format!(
                        "'{effect}': a bool takes true, false, a bool or params.<name>"
                    )),
                }
            }
            VarKind::List => Err(format!(
                "'{effect}': '{var}' is a list; use append() or remove_at()"
            )),
            VarKind::Str | VarKind::Num => Err(format!(
                "'{effect}': only counters and bools can be assigned, '{var}' is neither"
            )),
        },
        Effect::Append { list, value } => {
            if declared(list)? != VarKind::List {
                return Err(format!("'{effect}': '{list}' is not a list"));
            }
            match value {
                Arg::Lit(Literal::Str(_)) => Ok(None),
                Arg::Param(p) => Ok(Some((p.clone(), ParamKind::Str))),
                _ => Err(format!(
                    "'{effect}': lists hold strings; append a 'string' or params.<name>"
                )),
            }
        }
        Effect::RemoveAt { list, index } => {
            if declared(list)? != VarKind::List {
                return Err(format!("'{effect}': '{list}' is not a list"));
            }
            match index {
                Arg::Lit(Literal::Int(n)) if *n >= 0 => Ok(None),
                Arg::Param(p) => Ok(Some((p.clone(), ParamKind::Count))),
                _ => Err(format!(
                    "'{effect}': the index is a non-negative integer or params.<name>"
                )),
            }
        }
        Effect::Spawn { id: Some(id), .. } => match id {
            Arg::Lit(Literal::Str(_)) => Ok(None),
            Arg::Param(p) => Ok(Some((p.clone(), ParamKind::Str))),
            _ => Err(format!(
                "'{effect}': a child id is a 'string' or params.<name>"
            )),
        },
        Effect::Schedule { .. } | Effect::ScheduleAt { .. } | Effect::Spawn { id: None, .. } => {
            Ok(None)
        }
    }
}

/// Check an action's effects and return the kind of every parameter they
/// read. A parameter read as two different kinds is an error.
pub fn check_effects(
    effects: &[Effect],
    vars: &BTreeMap<String, VarKind>,
) -> Result<BTreeMap<String, ParamKind>, String> {
    let mut params = BTreeMap::new();
    for effect in effects {
        if let Some((param, kind)) = check_effect(effect, vars)? {
            match params.insert(param.clone(), kind) {
                Some(previous) if previous != kind => {
                    return Err(format!(
                        "params.{param} is read as both {previous:?} and {kind:?}"
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(params)
}
