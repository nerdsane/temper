//! Canonical source rendering. `parse(&expr.to_string()) == expr` for every
//! expression the parser produces.

use std::fmt;

use super::ast::{Expr, Literal, Operand, Set};

/// Binding strength, loosest first.
fn precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Implies(..) => 0,
        Expr::Or(_) => 1,
        Expr::And(_) => 2,
        Expr::Not(_) => 3,
        _ => 4,
    }
}

fn write_child(f: &mut fmt::Formatter<'_>, child: &Expr, min: u8) -> fmt::Result {
    if precedence(child) < min {
        write!(f, "({child})")
    } else {
        write!(f, "{child}")
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Const(value) => write!(f, "{value}"),
            Expr::Not(inner) => {
                f.write_str("!")?;
                write_child(f, inner, 3)
            }
            Expr::And(parts) | Expr::Or(parts) => {
                let (sep, min) = if matches!(self, Expr::And(_)) {
                    (" && ", 3)
                } else {
                    (" || ", 2)
                };
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        f.write_str(sep)?;
                    }
                    write_child(f, part, min)?;
                }
                Ok(())
            }
            Expr::Implies(lhs, rhs) => {
                // Right-associative: only a nested implication on the left
                // needs parentheses.
                write_child(f, lhs, 1)?;
                f.write_str(" => ")?;
                write_child(f, rhs, 0)
            }
            Expr::Compare { lhs, op, rhs } => write!(f, "{lhs} {} {rhs}", op.as_str()),
            Expr::In {
                value,
                set,
                negated,
            } => {
                let keyword = if *negated { "not in" } else { "in" };
                write!(f, "{value} {keyword} {set}")
            }
            Expr::Empty(name) => write!(f, "empty({name})"),
            Expr::Var(name) => f.write_str(name),
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Status => f.write_str("status"),
            Operand::Var(name) => f.write_str(name),
            Operand::Len(name) => write!(f, "len({name})"),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } => write!(f, "{entity_type}[{id_field}].status"),
            Operand::Lit(lit) => write!(f, "{lit}"),
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Int(value) => write!(f, "{value}"),
            Literal::Str(value) => write!(f, "'{value}'"),
            Literal::Bool(value) => write!(f, "{value}"),
            Literal::Null => f.write_str("null"),
        }
    }
}

impl fmt::Display for Set {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Set::Var(name) => f.write_str(name),
            Set::List(items) => {
                f.write_str("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
        }
    }
}
