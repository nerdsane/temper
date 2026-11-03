//! Predicate expression tree.

/// A boolean predicate over an entity's status, state variables and fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// `true` or `false`.
    Const(bool),
    /// `!e`.
    Not(Box<Expr>),
    /// `a && b && ...` (at least two children).
    And(Vec<Expr>),
    /// `a || b || ...` (at least two children).
    Or(Vec<Expr>),
    /// `a => b`.
    Implies(Box<Expr>, Box<Expr>),
    /// `lhs <op> rhs`.
    Compare {
        /// Left operand.
        lhs: Operand,
        /// Comparison operator.
        op: CmpOp,
        /// Right operand.
        rhs: Operand,
    },
    /// `value in set` or `value not in set`.
    In {
        /// The value tested for membership.
        value: Operand,
        /// The set searched.
        set: Set,
        /// `true` for `not in`.
        negated: bool,
    },
    /// `empty(name)`: absent, null, `''` or `[]`.
    Empty(String),
    /// A bare boolean name, such as `ready`.
    Var(String),
}

/// A value-producing term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// The entity's lifecycle status.
    Status,
    /// A state variable or field by name.
    Var(String),
    /// `len(name)`: the length of a list.
    Len(String),
    /// `Type[id_field].status`: the status of a related entity.
    CrossStatus {
        /// Related entity type.
        entity_type: String,
        /// Field on this entity holding the related entity's id.
        id_field: String,
    },
    /// `Type[id_field].field`: a field of a related entity. Only the joint
    /// verification of several entities reads it (an `[[invariant]]` over
    /// two records); a guard or field invariant may not.
    CrossField {
        /// Related entity type.
        entity_type: String,
        /// Field on this entity holding the related entity's id.
        id_field: String,
        /// The related entity's field.
        field: String,
    },
    /// `params.p`: the parameter `p` of the move being taken. Only an
    /// action's guard reads one, and only a parameter the action declares.
    Param(String),
    /// A literal value.
    Lit(Literal),
}

/// A literal value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// An integer.
    Int(i64),
    /// A single-quoted string.
    Str(String),
    /// `true` or `false` used as a value.
    Bool(bool),
    /// `null`: an absent value.
    Null,
}

/// The right-hand side of `in`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Set {
    /// `[lit, lit, ...]`.
    List(Vec<Literal>),
    /// A list-valued state variable or field.
    Var(String),
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

impl CmpOp {
    /// Source spelling of the operator.
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

impl Expr {
    /// The always-true predicate (an absent guard).
    pub fn always() -> Expr {
        Expr::Const(true)
    }

    /// Whether this is the always-true predicate.
    pub fn is_always(&self) -> bool {
        matches!(self, Expr::Const(true))
    }

    /// Conjunction that flattens nested `And`s and drops `true`.
    pub fn and(parts: Vec<Expr>) -> Expr {
        let mut flat = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                Expr::And(inner) => flat.extend(inner),
                Expr::Const(true) => {}
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => Expr::Const(true),
            1 => flat.pop().unwrap_or(Expr::Const(true)),
            _ => Expr::And(flat),
        }
    }

    /// Visit every operand and bare name in the expression.
    pub fn for_each_name(&self, visit: &mut impl FnMut(Name<'_>)) {
        match self {
            Expr::Const(_) => {}
            Expr::Not(inner) => inner.for_each_name(visit),
            Expr::And(parts) | Expr::Or(parts) => {
                for part in parts {
                    part.for_each_name(visit);
                }
            }
            Expr::Implies(a, b) => {
                a.for_each_name(visit);
                b.for_each_name(visit);
            }
            Expr::Compare { lhs, rhs, .. } => {
                lhs.for_each_name(visit);
                rhs.for_each_name(visit);
            }
            Expr::In { value, set, .. } => {
                value.for_each_name(visit);
                if let Set::Var(name) = set {
                    visit(Name::Var(name));
                }
            }
            Expr::Empty(name) | Expr::Var(name) => visit(Name::Var(name)),
        }
    }

    /// The statuses the expression allows, read from its top-level
    /// `status == 'S'` and `status in ['A', ...]` conjuncts (intersected when
    /// several pin `status`). `None` when no conjunct pins `status`.
    pub fn required_statuses(&self) -> Option<std::collections::BTreeSet<&str>> {
        let conjuncts = match self {
            Expr::And(parts) => parts.as_slice(),
            other => std::slice::from_ref(other),
        };
        let mut allowed: Option<std::collections::BTreeSet<&str>> = None;
        for conjunct in conjuncts {
            let statuses: std::collections::BTreeSet<&str> = match conjunct {
                Expr::Compare {
                    lhs: Operand::Status,
                    op: CmpOp::Eq,
                    rhs: Operand::Lit(Literal::Str(status)),
                } => std::iter::once(status.as_str()).collect(),
                Expr::In {
                    value: Operand::Status,
                    set: Set::List(items),
                    negated: false,
                } => items
                    .iter()
                    .filter_map(|item| match item {
                        Literal::Str(status) => Some(status.as_str()),
                        _ => None,
                    })
                    .collect(),
                _ => continue,
            };
            allowed = Some(match allowed {
                Some(previous) => previous.intersection(&statuses).copied().collect(),
                None => statuses,
            });
        }
        allowed
    }

    /// Every related-entity status the expression reads, deduplicated in
    /// first-use order. The runtime resolves these before evaluation.
    pub fn cross_refs(&self) -> Vec<(String, String)> {
        let mut refs: Vec<(String, String)> = Vec::new();
        self.for_each_name(&mut |name| {
            if let Name::CrossStatus {
                entity_type,
                id_field,
            } = name
            {
                let key = (entity_type.to_string(), id_field.to_string());
                if !refs.contains(&key) {
                    refs.push(key);
                }
            }
        });
        refs
    }

    /// Every related entity the expression reads, status or field, as
    /// `(entity_type, id_field)`, deduplicated in first-use order.
    pub fn related_types(&self) -> Vec<(String, String)> {
        let mut refs: Vec<(String, String)> = Vec::new();
        self.for_each_name(&mut |name| {
            let key = match name {
                Name::CrossStatus {
                    entity_type,
                    id_field,
                }
                | Name::CrossField {
                    entity_type,
                    id_field,
                    ..
                } => (entity_type.to_string(), id_field.to_string()),
                _ => return,
            };
            if !refs.contains(&key) {
                refs.push(key);
            }
        });
        refs
    }

    /// The parameters the expression reads (`params.p`), deduplicated in
    /// first-use order.
    pub fn params_read(&self) -> Vec<String> {
        let mut params: Vec<String> = Vec::new();
        self.for_each_name(&mut |name| {
            if let Name::Param(param) = name
                && !params.iter().any(|p| p == param)
            {
                params.push(param.to_string());
            }
        });
        params
    }

    /// Whether the expression reads a related entity's field
    /// (`Type[id].field`).
    pub fn reads_related_field(&self) -> bool {
        let mut found = false;
        self.for_each_name(&mut |name| {
            found |= matches!(name, Name::CrossField { .. });
        });
        found
    }
}

impl Operand {
    fn for_each_name(&self, visit: &mut impl FnMut(Name<'_>)) {
        match self {
            Operand::Status => visit(Name::Status),
            Operand::Var(name) | Operand::Len(name) => visit(Name::Var(name)),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } => {
                visit(Name::Var(id_field));
                visit(Name::CrossStatus {
                    entity_type,
                    id_field,
                });
            }
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } => {
                visit(Name::Var(id_field));
                visit(Name::CrossField {
                    entity_type,
                    id_field,
                    field,
                });
            }
            Operand::Param(name) => visit(Name::Param(name)),
            Operand::Lit(_) => {}
        }
    }
}

/// A name read by an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Name<'a> {
    /// `status`.
    Status,
    /// A state variable or field.
    Var(&'a str),
    /// A related entity's status.
    CrossStatus {
        /// Related entity type.
        entity_type: &'a str,
        /// Field holding the related entity's id.
        id_field: &'a str,
    },
    /// A related entity's field.
    CrossField {
        /// Related entity type.
        entity_type: &'a str,
        /// Field holding the related entity's id.
        id_field: &'a str,
        /// The related entity's field.
        field: &'a str,
    },
    /// `params.p`, a parameter of the move being taken.
    Param(&'a str),
}
