//! Re-check a spec's invariants against the entities that actually exist.
//!
//! The verification cascade proves properties of a *model*: an abstraction in
//! which state variables are counters, bools and lists, every write comes from
//! a declared effect, and the reachable state space is whatever those effects
//! generate. That proof is only about the real system to the extent the model
//! matches it, and there are three places it does not:
//!
//!   * **Fields, which the cascade refuses to model at all.** An `[[invariant]]`
//!     may not read a field -- the parser rejects it and directs the author to
//!     `[[field_invariant]]`, which is checked *on writes*. That is a real
//!     check, and it is also the narrowest possible one: it sees a row only
//!     when something writes to it. A field invariant added today has never
//!     been evaluated against a single row written yesterday, and never will
//!     be until something happens to touch one.
//!   * **State that predates the current spec.** Entities persist across spec
//!     edits. Tightening an invariant proves the *new* spec consistent; it says
//!     nothing about rows written under the old one, which keep whatever they
//!     had.
//!   * **Liveness the runtime does not enforce.** `[[state_timeout]]` is a
//!     declaration, not a watchdog. An entity whose callback never arrives sits
//!     in a transient state indefinitely and no level of the cascade is looking.
//!
//! So this module asks a different question than the cascade does. Not "can the
//! model reach a bad state" but "is any entity in one now". It is deliberately
//! cheap and total: every invariant against every entity, using the same
//! evaluator the runtime uses to admit writes, so a violation here is a
//! violation by the runtime's own definition rather than a modelling artifact.
//!
//! Findings are observations, not proofs of a bug. A violated invariant means
//! the spec and the data disagree, and which one is wrong is a judgement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_spec::automaton::Automaton;

use crate::table::action_contract::InitialValues;
use crate::table::guard::{self, EvalContext};

/// How much a finding should worry the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditSeverity {
    /// The data contradicts the spec.
    Violation,
    /// The data is legal but suspicious.
    Warning,
}

/// One thing the audit noticed about one entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFinding {
    /// The entity this is about.
    pub entity_id: String,
    /// Stable code for tooling and CI.
    pub code: String,
    /// Violation or warning.
    pub severity: AuditSeverity,
    /// The entity's status when read.
    pub status: String,
    /// What is wrong, in one line.
    pub message: String,
    /// The names the failing expression read and the values found, so the
    /// reader does not have to re-query the entity to interpret the finding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub found: Vec<(String, String)>,
}

/// One entity's persisted state.
///
/// Mirrors the nested shape `GET /tdata/{EntitySet}` returns, which is the one
/// that carries `counters`, `booleans`, `lists` and `fields` separately. A flat
/// OData projection cannot be audited: the separation is what tells the
/// evaluator whether `count` is a counter or a field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntitySnapshot {
    /// The entity's id.
    pub entity_id: String,
    /// Its lifecycle status.
    pub status: String,
    /// Counter state variables.
    #[serde(default)]
    pub counters: BTreeMap<String, usize>,
    /// Boolean state variables.
    #[serde(default)]
    pub booleans: BTreeMap<String, bool>,
    /// List and set state variables.
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Ordinary fields, including every value a WASM module wrote.
    #[serde(default)]
    pub fields: BTreeMap<String, Value>,
}

impl EntitySnapshot {
    /// Read one entity out of a `/tdata` collection row.
    ///
    /// Missing sub-objects decode as empty rather than failing: a legacy
    /// catalog row carries `status` and `fields` but no counters, and auditing
    /// its invariants on fields alone is worth more than refusing to look.
    /// Only a missing `status` is fatal, because every check needs it.
    pub fn from_tdata_row(row: &Value) -> Result<Self, String> {
        let status = row
            .get("status")
            .and_then(Value::as_str)
            .ok_or("row has no `status`")?
            .to_string();
        let entity_id = row
            .get("entity_id")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>")
            .to_string();
        let sub = |key: &str| {
            row.get(key)
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()))
        };
        let mut counters: BTreeMap<String, usize> =
            serde_json::from_value(sub("counters")).unwrap_or_default();
        // Match the runtime's legacy `items` fallback; named counters win.
        if let Some(count) = row.get("item_count").and_then(Value::as_u64)
            && let Ok(count) = usize::try_from(count)
        {
            counters.entry("items".to_string()).or_insert(count);
        }
        Ok(Self {
            entity_id,
            status,
            counters,
            booleans: serde_json::from_value(sub("booleans")).unwrap_or_default(),
            lists: serde_json::from_value(sub("lists")).unwrap_or_default(),
            fields: serde_json::from_value(sub("fields")).unwrap_or_default(),
        })
    }

    fn eval_context(&self) -> EvalContext {
        EvalContext {
            counters: self.counters.clone(),
            booleans: self.booleans.clone(),
            lists: self.lists.clone(),
            fields: self.fields.clone(),
            // Cross-entity references are not resolved: the audit reads one
            // entity set at a time. An invariant that reads one is skipped
            // rather than evaluated against a missing status -- see
            // `audit_entity`, which says so in its findings.
            related: BTreeMap::new(),
            // Invariants read no move's parameters.
            params: serde_json::Map::new(),
        }
    }
}

/// Re-check every invariant, and the derived liveness checks, against one entity.
pub fn audit_entity(automaton: &Automaton, snapshot: &EntitySnapshot) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let declared = InitialValues::from_declarations(&automaton.state);
    let ctx = snapshot.eval_context();

    let finding =
        |code: &str, severity, message: String, found: Vec<(String, String)>| AuditFinding {
            entity_id: snapshot.entity_id.clone(),
            code: code.to_string(),
            severity,
            status: snapshot.status.clone(),
            message,
            found,
        };

    // An undeclared status is checked first and alone. Every other check reads
    // the status, so reporting them too would be a list of consequences of
    // this one finding.
    if !automaton.automaton.states.contains(&snapshot.status) {
        findings.push(finding(
            "status_undeclared",
            AuditSeverity::Violation,
            format!(
                "status '{}' is not a declared state of '{}'; the entity predates \
                 a spec change that removed or renamed it",
                snapshot.status, automaton.automaton.name
            ),
            vec![],
        ));
        return findings;
    }

    for invariant in &automaton.invariants {
        // A cross-entity read is unknown here, not false, and `check_detailed`
        // cannot tell those apart -- it reports any non-true as a failure. An
        // invariant we cannot evaluate must not be reported as violated, so it
        // is skipped and said to be skipped.
        if !invariant.assert.cross_refs().is_empty() {
            findings.push(finding(
                "invariant_not_evaluated",
                AuditSeverity::Warning,
                format!(
                    "invariant '{}' reads another entity's status, which this audit \
                     does not resolve; it was not checked",
                    invariant.name
                ),
                vec![],
            ));
            continue;
        }
        if let Some(failure) =
            guard::check_detailed(&invariant.assert, &snapshot.status, &ctx, &declared)
        {
            findings.push(finding(
                "invariant_violated",
                AuditSeverity::Violation,
                format!(
                    "invariant '{}' does not hold: {} is false",
                    invariant.name, failure.expr
                ),
                failure.found,
            ));
        }
    }

    // Field invariants, which the runtime checks on writes and therefore has
    // never checked on a row nobody has written to since the rule was added.
    // This is the only thing that evaluates them against the existing corpus.
    for invariant in &automaton.field_invariants {
        if !invariant.assert.cross_refs().is_empty() {
            findings.push(finding(
                "field_invariant_not_evaluated",
                AuditSeverity::Warning,
                format!(
                    "field_invariant '{}' reads another entity's status, which this \
                     audit does not resolve; it was not checked",
                    invariant.name
                ),
                vec![],
            ));
            continue;
        }
        if let Some(failure) =
            guard::check_detailed(&invariant.assert, &snapshot.status, &ctx, &declared)
        {
            findings.push(finding(
                "field_invariant_violated",
                AuditSeverity::Violation,
                match &invariant.message {
                    Some(message) => format!(
                        "field_invariant '{}' does not hold: {} is false ({message})",
                        invariant.name, failure.expr
                    ),
                    None => format!(
                        "field_invariant '{}' does not hold: {} is false",
                        invariant.name, failure.expr
                    ),
                },
                failure.found,
            ));
        }
    }

    findings.extend(audit_liveness(automaton, snapshot, &ctx, &declared));
    findings
}

/// The two ways a live entity can be stuck, which no cascade level can see
/// because both are about dwelling rather than transitioning.
fn audit_liveness(
    automaton: &Automaton,
    snapshot: &EntitySnapshot,
    ctx: &EvalContext,
    declared: &InitialValues,
) -> Vec<AuditFinding> {
    let mut findings = Vec::new();
    let meta = &automaton.automaton;
    // Two kinds of resting place, both deliberate. `terminal` says no action
    // may leave; `allow_indefinite_states` says an entity may sit here as long
    // as it likes. Neither is stuck, and in an indefinite state having no
    // outgoing action at all is the spec working as written rather than a
    // dead end -- reporting it as one would make the audit fire on every
    // healthy entity in the set, which is how a check gets ignored.
    if meta.terminal.contains(&snapshot.status)
        || meta.allow_indefinite_states.contains(&snapshot.status)
    {
        return findings;
    }

    let mut unresolved = Vec::new();
    let enabled: Vec<&str> = automaton
        .actions
        .iter()
        .filter(|action| {
            let from_ok = if action.from.is_empty() {
                action.kind.enabled_everywhere()
            } else {
                action.from.contains(&snapshot.status)
            };
            if !from_ok {
                return false;
            }
            if !action.guard.cross_refs().is_empty() {
                unresolved.push(action.name.as_str());
                return false;
            }
            guard::check(&action.guard, &snapshot.status, ctx, declared)
        })
        .map(|action| action.name.as_str())
        .collect();

    if enabled.is_empty() && !unresolved.is_empty() {
        findings.push(AuditFinding {
            entity_id: snapshot.entity_id.clone(),
            code: "liveness_not_evaluated".to_string(),
            severity: AuditSeverity::Warning,
            status: snapshot.status.clone(),
            message: format!(
                "cannot determine whether state '{}' is stranded: {} read other entities, which this audit does not resolve",
                snapshot.status,
                unresolved.join(", ")
            ),
            found: vec![],
        });
    } else if enabled.is_empty() {
        // Strictly worse than sitting in a transient state: there is no legal
        // move out, so no retry, callback or operator action can help. The
        // entity is finished without being in a state declared as finished.
        findings.push(AuditFinding {
            entity_id: snapshot.entity_id.clone(),
            code: "entity_stranded".to_string(),
            severity: AuditSeverity::Violation,
            status: snapshot.status.clone(),
            message: format!(
                "no action is enabled in non-terminal state '{}', so the entity \
                 cannot progress; a guard excludes every outgoing action",
                snapshot.status
            ),
            found: vec![],
        });
    } else {
        // A state the spec does not list as indefinite is meant to be passed
        // through. Nothing enforces that at runtime, so an entity whose
        // callback never arrived waits here forever and looks healthy.
        //
        // A warning, and a point-in-time one: a snapshot cannot distinguish
        // "transient, caught mid-flight" from "abandoned". Repeat the audit --
        // the same entity in the same transient state twice is the signal.
        findings.push(AuditFinding {
            entity_id: snapshot.entity_id.clone(),
            code: "transient_state_occupied".to_string(),
            severity: AuditSeverity::Warning,
            status: snapshot.status.clone(),
            message: format!(
                "state '{}' is neither terminal nor declared indefinite, so it is \
                 meant to be transient; {} could still fire, but nothing will make it",
                snapshot.status,
                enabled.join(", ")
            ),
            found: vec![],
        });
    }
    findings
}

/// What an audit of a whole entity set found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReport {
    /// The automaton audited.
    pub entity_type: String,
    /// How many entities were read.
    pub entities_audited: usize,
    /// How many of them had at least one `Violation`.
    pub entities_violating: usize,
    /// Every finding, in entity order.
    pub findings: Vec<AuditFinding>,
}

impl AuditReport {
    /// Whether any finding is a violation rather than a warning.
    pub fn has_violations(&self) -> bool {
        self.findings
            .iter()
            .any(|f| f.severity == AuditSeverity::Violation)
    }
}

/// Audit every entity in a set against one automaton.
pub fn audit_entities(automaton: &Automaton, snapshots: &[EntitySnapshot]) -> AuditReport {
    let mut findings = Vec::new();
    let mut entities_violating = 0;
    for snapshot in snapshots {
        let entity = audit_entity(automaton, snapshot);
        if entity
            .iter()
            .any(|f| f.severity == AuditSeverity::Violation)
        {
            entities_violating += 1;
        }
        findings.extend(entity);
    }
    AuditReport {
        entity_type: automaton.automaton.name.clone(),
        entities_audited: snapshots.len(),
        entities_violating,
        findings,
    }
}

#[cfg(test)]
#[path = "audit_test.rs"]
mod tests;
