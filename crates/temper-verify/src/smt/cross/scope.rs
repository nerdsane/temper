//! What one cross-record rule's check needs from the specs: the moves that can
//! change what the rule reads, what sends them, and the fields they touch.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{
    Action, ActionKind, ActionTrigger, Automaton, Effect, TargetResolver, TriggerKind,
};
use temper_spec::predicate::{Arg, Expr, Name};

use super::{CrossRule, MaySend};

/// How a reaction finds the record it reaches.
#[derive(Debug, Clone)]
pub(super) enum Target {
    /// The record named by the source's field; a missing one takes nothing.
    Field(String),
    /// The source itself.
    Same,
    /// A fixed id.
    Static(String),
    /// The record named by the source's field, made if missing. An empty
    /// field gets a new id.
    MakeAt(String),
    /// A new record.
    Make,
    /// A spawn: a new or named record whose id the source stores at commit.
    Spawn {
        /// The source's field holding the child's id.
        store: Option<String>,
        /// The child's id, when the spawn names one.
        id: Option<Arg>,
    },
}

/// One reaction a move may send, a trigger or a spawn.
#[derive(Debug, Clone)]
pub(super) struct Send<'a> {
    /// The trigger's name, or `spawn`.
    pub name: &'a str,
    /// The target type.
    pub to: &'a str,
    /// The target action.
    pub action: &'a str,
    /// When it fires, read over the source after it commits.
    pub guard: Option<&'a Expr>,
    /// What it passes.
    pub args: Vec<(String, Arg)>,
    /// A spawn also hands over the parameters of the move that made it.
    pub passes_params: bool,
    /// Which record it reaches.
    pub target: Target,
}

/// The reactions `action` may send to other records.
pub(super) fn sends_of<'a>(source: &'a str, action: &'a Action) -> Vec<Send<'a>> {
    let mut sends: Vec<Send<'a>> = action
        .triggers
        .iter()
        .filter(|t| t.kind == TriggerKind::Entity)
        .filter_map(trigger_send)
        .collect();
    for effect in &action.effect {
        let Effect::Spawn {
            entity_type,
            initial_action,
            store_id_in,
            id,
            copy,
        } = effect
        else {
            continue;
        };
        let mut args: Vec<(String, Arg)> = copy
            .iter()
            .map(|name| (name.clone(), Arg::Var(name.clone())))
            .collect();
        args.push((
            "parent_type".into(),
            Arg::Lit(temper_spec::predicate::Literal::Str(source.into())),
        ));
        args.push(("parent_id".into(), Arg::Var("Id".into())));
        sends.push(Send {
            name: "spawn",
            to: entity_type,
            action: initial_action,
            guard: None,
            args,
            passes_params: true,
            target: Target::Spawn {
                store: store_id_in.clone(),
                id: id.clone(),
            },
        });
    }
    sends
}

fn trigger_send(t: &ActionTrigger) -> Option<Send<'_>> {
    let target = match t.resolve_target.as_ref()? {
        TargetResolver::Field { id_field } => Target::Field(id_field.clone()),
        TargetResolver::SameId => Target::Same,
        TargetResolver::Static { entity_id } => Target::Static(entity_id.clone()),
        TargetResolver::CreateIfMissing { id_field } => Target::MakeAt(id_field.clone()),
        TargetResolver::Create => Target::Make,
    };
    Some(Send {
        name: &t.name,
        to: t.target_entity.as_deref()?,
        action: t.target_action.as_deref()?,
        guard: t.guard.as_ref(),
        args: t.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        passes_params: false,
        target,
    })
}

/// The fields an action writes, parameters included.
fn writes_of(automaton: &Automaton, action: &Action) -> BTreeSet<String> {
    let mut written = BTreeSet::new();
    for effect in &action.effect {
        match effect {
            Effect::Assign { var, .. } => {
                written.insert(var.clone());
            }
            Effect::Append { list, .. }
            | Effect::RemoveAt { list, .. }
            | Effect::Remove { list, .. } => {
                written.insert(list.clone());
            }
            Effect::Spawn {
                store_id_in: Some(field),
                ..
            } => {
                written.insert(field.clone());
            }
            _ => {}
        }
    }
    for param in &action.params {
        if automaton.state.iter().any(|v| v.name == param.name()) {
            written.insert(param.name().to_string());
        }
    }
    for trigger in &action.triggers {
        if let Some(TargetResolver::CreateIfMissing { id_field }) = &trigger.resolve_target {
            written.insert(id_field.clone());
        }
    }
    written
}

/// For each status, the fields a record there may have moved off their initial
/// value: what its moves on the way there write.
pub(super) fn written_by_status(a: &Automaton) -> BTreeMap<String, BTreeSet<String>> {
    let mut at: BTreeMap<String, BTreeSet<String>> = a
        .automaton
        .states
        .iter()
        .map(|s| (s.clone(), BTreeSet::new()))
        .collect();
    let writes: Vec<(&Action, BTreeSet<String>)> =
        a.actions.iter().map(|x| (x, writes_of(a, x))).collect();
    loop {
        let mut changed = false;
        for (action, written) in &writes {
            for from in froms(a, action) {
                let Some(before) = at.get(&from).cloned() else {
                    continue;
                };
                let to = action.to.clone().unwrap_or_else(|| from.clone());
                if let Some(after) = at.get_mut(&to) {
                    let size = after.len();
                    after.extend(before);
                    after.extend(written.iter().cloned());
                    changed |= after.len() != size;
                }
            }
        }
        if !changed {
            return at;
        }
    }
}

fn froms(a: &Automaton, action: &Action) -> Vec<String> {
    if action.from.is_empty() {
        a.automaton.states.clone()
    } else {
        action.from.clone()
    }
}

/// The fields `expr` reads on its own record.
pub(super) fn own_reads(expr: &Expr, out: &mut BTreeSet<String>) {
    expr.for_each_name(&mut |name| {
        if let Name::Var(field) = name {
            out.insert(field.to_string());
        }
    });
}

/// Fields of other types `expr` reads, by type.
fn related_reads(expr: &Expr, out: &mut BTreeMap<String, BTreeSet<String>>) {
    expr.for_each_name(&mut |name| {
        if let Name::CrossField {
            entity_type, field, ..
        } = name
        {
            out.entry(entity_type.to_string())
                .or_default()
                .insert(field.to_string());
        }
    });
}

/// The types whose status `rule` reads, and the fields it reads by type.
fn supports(owner: &str, rule: &Expr) -> (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>) {
    let mut own = BTreeSet::new();
    own_reads(rule, &mut own);
    let mut related = BTreeMap::new();
    related_reads(rule, &mut related);
    let mut read_status: BTreeSet<String> = rule
        .related_types()
        .into_iter()
        .map(|(entity_type, _)| entity_type)
        .collect();
    read_status.insert(owner.to_string());
    related.entry(owner.to_string()).or_default().extend(own);
    (read_status, related)
}

/// `seeds` and whatever sends one of them, however far back.
pub(super) fn senders_of(
    specs: &BTreeMap<String, Automaton>,
    seeds: &BTreeSet<(String, String)>,
) -> BTreeSet<(String, String)> {
    let mut all = seeds.clone();
    loop {
        let mut grew = false;
        for (ty, spec) in specs {
            for action in &spec.actions {
                let key = (ty.clone(), action.name.clone());
                if !all.contains(&key)
                    && sends_of(ty, action)
                        .iter()
                        .any(|send| all.contains(&(send.to.to_string(), send.action.to_string())))
                {
                    all.insert(key);
                    grew = true;
                }
            }
        }
        if !grew {
            return all;
        }
    }
}

/// What one rule's check works over.
pub(super) struct Scope<'a> {
    /// Every spec, by type.
    pub specs: &'a BTreeMap<String, Automaton>,
    /// The moves that can reach what the rule reads.
    pub relevant: BTreeSet<(String, String)>,
    /// The fields kept for each type.
    pub fields: BTreeMap<String, BTreeSet<String>>,
    /// The moves that start on their own: a request, a timer, a module answer.
    pub roots: Vec<(String, String)>,
    /// Every relevant reaction, as (source type, source action, its index in
    /// [`sends_of`]).
    pub edges: Vec<(String, String, usize)>,
    /// The types the rule reads, each with its own records in the world.
    pub types: BTreeSet<String>,
}

impl<'a> Scope<'a> {
    /// Everything that writes what `rule` on `owner` reads, one move each,
    /// to ask which moves can change its truth.
    pub fn broad(specs: &'a BTreeMap<String, Automaton>, owner: &str, rule: &Expr) -> Self {
        let (read_status, support) = supports(owner, rule);
        let mut relevant = BTreeSet::new();
        for (ty, spec) in specs {
            let read = support.get(ty);
            for action in &spec.actions {
                let makes = froms(spec, action).contains(&spec.automaton.initial);
                let moves = read_status.contains(ty) && (action.to.is_some() || makes);
                let writes = read.is_some_and(|fields| {
                    writes_of(spec, action).iter().any(|f| fields.contains(f))
                });
                if moves || writes {
                    relevant.insert((ty.clone(), action.name.clone()));
                }
            }
        }
        Self::over(specs, owner, rule, relevant, None, &[])
    }

    /// The moves `seeds` and everything that sends them, however far back.
    /// `may_send` says whether someone outside may send a move as a request;
    /// every input may without it.
    pub fn narrowed(
        specs: &'a BTreeMap<String, Automaton>,
        owner: &str,
        rule: &Expr,
        seeds: &BTreeSet<(String, String)>,
        may_send: Option<MaySend<'_>>,
        assumed: &[CrossRule],
    ) -> Self {
        Self::over(
            specs,
            owner,
            rule,
            senders_of(specs, seeds),
            may_send,
            assumed,
        )
    }

    fn over(
        specs: &'a BTreeMap<String, Automaton>,
        owner: &str,
        rule: &Expr,
        relevant: BTreeSet<(String, String)>,
        may_send: Option<MaySend<'_>>,
        assumed: &[CrossRule],
    ) -> Self {
        let (read_status, mut support) = supports(owner, rule);
        for other in assumed {
            let (_, reads) = supports(&other.owner, &other.assert);
            for (ty, fields) in reads {
                support.entry(ty).or_default().extend(fields);
            }
        }
        let mut scope = Scope {
            specs,
            relevant,
            fields: BTreeMap::new(),
            roots: Vec::new(),
            edges: Vec::new(),
            types: read_status,
        };
        scope.fields_and_edges(rule, owner, &support);
        scope.find_roots(may_send);
        scope
    }

    fn fields_and_edges(
        &mut self,
        rule: &Expr,
        owner: &str,
        support: &BTreeMap<String, BTreeSet<String>>,
    ) {
        let mut fields = support.clone();
        fields.entry(owner.to_string()).or_default();
        let mut own = BTreeSet::new();
        own_reads(rule, &mut own);
        fields.entry(owner.to_string()).or_default().extend(own);
        let relevant: Vec<_> = self.relevant.iter().cloned().collect();
        for (ty, name) in &relevant {
            let spec = &self.specs[ty];
            let Some(action) = spec.actions.iter().find(|a| &a.name == name) else {
                continue;
            };
            let mut mine = fields.remove(ty).unwrap_or_default();
            own_reads(&action.guard, &mut mine);
            for constraint in &action.constraints {
                if let Some(field) = constraint.field() {
                    mine.insert(field.to_string());
                }
            }
            for effect in &action.effect {
                if let Effect::Assign {
                    value: Arg::Var(var),
                    ..
                } = effect
                {
                    mine.insert(var.clone());
                }
            }
            mine.extend(writes_of(spec, action));
            for (index, send) in sends_of(ty, action).iter().enumerate() {
                if !self
                    .relevant
                    .contains(&(send.to.to_string(), send.action.to_string()))
                {
                    continue;
                }
                self.edges.push((ty.clone(), name.clone(), index));
                reads_of_send(send, &mut mine);
            }
            fields.insert(ty.clone(), mine);
        }
        // The installed rules of one record hold in what is assumed.
        for (ty, mine) in &mut fields {
            if let Some(spec) = self.specs.get(ty) {
                for inv in &spec.invariants {
                    if inv.assert.related_types().is_empty() {
                        own_reads(&inv.assert, mine);
                    }
                }
            }
            mine.remove("Id");
            mine.remove("id");
        }
        self.fields = fields;
    }

    fn find_roots(&mut self, may_send: Option<MaySend<'_>>) {
        for (ty, name) in &self.relevant {
            let spec = &self.specs[ty];
            let Some(action) = spec.actions.iter().find(|a| &a.name == name) else {
                continue;
            };
            let outside = match action.kind {
                ActionKind::Input | ActionKind::Composite => {
                    may_send.is_none_or(|may| may(ty, name))
                }
                _ => false,
            };
            if outside || kernel_sends(spec, name) {
                self.roots.push((ty.clone(), name.clone()));
            }
        }
    }

    /// The action `name` of `ty`.
    pub fn action(&self, ty: &str, name: &str) -> Option<&'a Action> {
        self.specs.get(ty)?.actions.iter().find(|a| a.name == name)
    }
}

/// Whether the kernel sends `name` on its own: a timer or a module's answer.
fn kernel_sends(spec: &Automaton, name: &str) -> bool {
    spec.state_timeouts.iter().any(|t| t.on_timeout == name)
        || spec.actions.iter().any(|a| {
            a.effect.iter().any(|e| match e {
                Effect::Schedule { action, .. } | Effect::ScheduleAt { action, .. } => {
                    action == name
                }
                _ => false,
            }) || a.triggers.iter().any(|t| {
                t.on_success.iter().any(|s| s == name) || t.on_failure.as_deref() == Some(name)
            })
        })
}

/// The field a send keeps the id of the record it makes in.
pub(super) fn stored_field<'s>(send: &'s Send<'_>) -> Option<&'s String> {
    match &send.target {
        Target::MakeAt(field) => Some(field),
        Target::Spawn { store, .. } => store.as_ref(),
        _ => None,
    }
}

/// The source fields a reaction reads.
pub(super) fn reads_of_send(send: &Send<'_>, out: &mut BTreeSet<String>) {
    if let Some(guard) = send.guard {
        own_reads(guard, out);
    }
    for (_, arg) in &send.args {
        if let Arg::Var(var) = arg {
            out.insert(var.clone());
        }
    }
    match &send.target {
        Target::Field(field) | Target::MakeAt(field) => {
            out.insert(field.clone());
        }
        Target::Spawn { store, id } => {
            if let Some(field) = store {
                out.insert(field.clone());
            }
            if let Some(Arg::Var(var)) = id {
                out.insert(var.clone());
            }
        }
        Target::Same | Target::Static(_) | Target::Make => {}
    }
}
