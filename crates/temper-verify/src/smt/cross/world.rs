//! Symbolic records and how predicates read them.
//!
//! Every value is one Z3 integer. A string is an atom: `''` is 0, an absent
//! value is -1, and each literal has its own number, so strings only compare
//! for equality, which is all a predicate does with them. Counters and ints
//! are themselves and uncapped. A list is its length; what it holds is free.

use std::collections::BTreeMap;

use temper_spec::automaton::{Automaton, Initial};
use temper_spec::predicate::{CmpOp, Expr, Literal, Operand, Set};
use z3::ast::{Bool, Int};

/// The value of `''`.
pub(super) const EMPTY: i64 = 0;
/// The value of something absent.
pub(super) const ABSENT: i64 = -1;
const TRUE: i64 = 1;
const FIRST_ATOM: i64 = 1_000_000;

/// The numbers given to string literals.
#[derive(Default)]
pub(super) struct Atoms {
    numbers: BTreeMap<String, i64>,
    names: BTreeMap<i64, String>,
}

impl Atoms {
    /// The number of a string literal.
    pub fn atom(&mut self, text: &str) -> i64 {
        if text.is_empty() {
            return EMPTY;
        }
        if let Some(n) = self.numbers.get(text) {
            return *n;
        }
        let n = FIRST_ATOM + self.numbers.len() as i64;
        self.numbers.insert(text.to_string(), n);
        self.names.insert(n, text.to_string());
        n
    }

    /// A value as it reads in a counterexample.
    pub fn show(&self, value: i64) -> String {
        match value {
            EMPTY => "''".into(),
            ABSENT => "absent".into(),
            n => self
                .names
                .get(&n)
                .map_or_else(|| n.to_string(), |s| format!("'{s}'")),
        }
    }

    /// A literal's value.
    pub fn literal(&mut self, lit: &Literal) -> i64 {
        match lit {
            Literal::Int(n) => *n,
            Literal::Str(s) => self.atom(s),
            Literal::Bool(b) => i64::from(*b),
            Literal::Null => ABSENT,
        }
    }

    /// A spec's starting value.
    pub fn initial(&mut self, initial: &Initial) -> i64 {
        match initial {
            Initial::String(s) => self.atom(s),
            Initial::Bool(b) => i64::from(*b),
            Initial::Counter(n) => *n as i64,
            Initial::Int(n) => *n,
            Initial::List(items) => items.len() as i64,
        }
    }
}

/// One symbolic record at one point of a path.
#[derive(Clone)]
pub(super) struct Rec {
    /// Its name in a counterexample.
    pub name: String,
    /// Its type.
    pub ty: String,
    /// Its id.
    pub id: Int,
    /// Whether it has been made.
    pub exists: Bool,
    /// Its status.
    pub status: Int,
    /// The fields kept for its type.
    pub fields: BTreeMap<String, Int>,
}

impl Rec {
    /// A field's value; a field not kept is free.
    pub fn field(&self, name: &str) -> Int {
        if matches!(name, "Id" | "id") {
            return self.id.clone();
        }
        self.fields
            .get(name)
            .cloned()
            .unwrap_or_else(|| Int::fresh_const(&format!("{}.{name}", self.name)))
    }
}

/// Every record of a path at one point.
#[derive(Clone, Default)]
pub(super) struct World {
    /// The records, in a fixed order, so one record has one index on a path.
    pub records: Vec<Rec>,
    /// What a record of each type reads before it is made, for an id none of
    /// the records hold: the kernel reads any named record, made or not.
    pub unmade: BTreeMap<String, Rec>,
}

impl World {
    /// The indices of the records of `ty`.
    pub fn of(&self, ty: &str) -> Vec<usize> {
        (0..self.records.len())
            .filter(|i| self.records[*i].ty == ty)
            .collect()
    }

    /// `read` of the record of `ty` whose id is `id`. An empty reference names
    /// nothing and reads null; any other reads its record, made or not.
    fn lookup(&self, ty: &str, id: &Int, read: impl Fn(&Rec) -> Int) -> Int {
        let absent = Int::from_i64(ABSENT);
        let mut value = self.unmade.get(ty).map_or_else(|| absent.clone(), &read);
        for i in self.of(ty) {
            let rec = &self.records[i];
            value = id.eq(&rec.id).ite(&read(rec), &value);
        }
        let empty = Bool::or(&[id.eq(Int::from_i64(EMPTY)), id.eq(&absent)]);
        empty.ite(&absent, &value)
    }
}

/// What a predicate reads in one place.
pub(super) struct Reader<'a> {
    /// The record it is on.
    pub rec: &'a Rec,
    /// The other records.
    pub world: &'a World,
    /// The move's parameters, for a guard.
    pub params: Option<&'a BTreeMap<String, Int>>,
    /// The specs, for what kind a field is.
    pub spec: Option<&'a Automaton>,
}

impl Reader<'_> {
    /// The truth of `expr`.
    pub fn truth(&self, atoms: &mut Atoms, expr: &Expr) -> Bool {
        match expr {
            Expr::Const(b) => Bool::from_bool(*b),
            Expr::Not(inner) => self.truth(atoms, inner).not(),
            Expr::And(parts) => {
                let parts: Vec<Bool> = parts.iter().map(|p| self.truth(atoms, p)).collect();
                Bool::and(&parts)
            }
            Expr::Or(parts) => {
                let parts: Vec<Bool> = parts.iter().map(|p| self.truth(atoms, p)).collect();
                Bool::or(&parts)
            }
            Expr::Implies(a, b) => self.truth(atoms, a).implies(self.truth(atoms, b)),
            Expr::Compare { lhs, op, rhs } => {
                let (a, b) = (self.value(atoms, lhs), self.value(atoms, rhs));
                match op {
                    CmpOp::Eq => a.eq(&b),
                    CmpOp::Ne => a.eq(&b).not(),
                    CmpOp::Lt => a.lt(&b),
                    CmpOp::Le => a.le(&b),
                    CmpOp::Gt => a.gt(&b),
                    CmpOp::Ge => a.ge(&b),
                }
            }
            Expr::In {
                value,
                set,
                negated,
            } => {
                let inside = match set {
                    Set::List(items) => {
                        let value = self.value(atoms, value);
                        let parts: Vec<Bool> = items
                            .iter()
                            .map(|item| value.eq(Int::from_i64(atoms.literal(item))))
                            .collect();
                        Bool::or(&parts)
                    }
                    // What a list holds is free.
                    Set::Var(_) => Bool::fresh_const("in_list"),
                };
                if *negated { inside.not() } else { inside }
            }
            Expr::Empty(name) => {
                let value = self.rec.field(name);
                Bool::or(&[
                    value.eq(Int::from_i64(EMPTY)),
                    value.eq(Int::from_i64(ABSENT)),
                ])
            }
            Expr::Var(name) => self.rec.field(name).eq(Int::from_i64(TRUE)),
        }
    }

    fn value(&self, atoms: &mut Atoms, operand: &Operand) -> Int {
        match operand {
            Operand::Status => self.rec.status.clone(),
            Operand::Var(name) | Operand::Len(name) => self.rec.field(name),
            Operand::Param(name) => self
                .params
                .and_then(|params| params.get(name))
                .cloned()
                .unwrap_or_else(|| Int::from_i64(ABSENT)),
            Operand::Lit(lit) => Int::from_i64(atoms.literal(lit)),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } => self.related(entity_type, id_field, |rec| rec.status.clone()),
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } => self.related(entity_type, id_field, |rec| rec.field(field)),
        }
    }

    /// A read of the related record. One the world holds no record of, or
    /// named by a list of ids, is free.
    fn related(&self, ty: &str, id_field: &str, read: impl Fn(&Rec) -> Int) -> Int {
        let list = self.spec.is_some_and(|spec| {
            spec.state
                .iter()
                .any(|v| v.name == id_field && matches!(v.initial, Initial::List(_)))
        });
        if list || self.world.of(ty).is_empty() {
            return Int::fresh_const("unread");
        }
        let id = self.rec.field(id_field);
        self.world.lookup(ty, &id, read)
    }
}
