//! What one OTS decision contributes to the action walk.
//!
//! A decision's `choice.action` is not always an action name. Four shapes reach
//! the checker:
//!
//! - a thought — a reasoning step or a response formulation, whose `action` is
//!   free-form prose ([`temper_ots::models::DecisionType::is_invocation`]);
//! - a **harness envelope** — an invocation of the harness's own tool rather
//!   than of a governed action. The MCP server records every `execute` tool
//!   call as one decision whose `choice.action` is `"execute: <the submitted
//!   code>"` (`temper_mcp::runtime::record_execute_turn`), with the governed
//!   actions the code calls listed under `choice.arguments.trajectory_actions`;
//! - a **harness tool** — `Bash`, `Read`, `mcp__linear__list_issues`. A
//!   transcript-derived trajectory writes one decision per tool call with the
//!   tool's own name as `choice.action`, so most decisions in an agent-harness
//!   run are these;
//! - an action name, which is what the checker can judge.
//!
//! Reading the first three as action names reports attempts the agent never
//! made: one violation naming a hundred characters of Python per MCP turn, and
//! one per harness tool call for every `Bash` and `Read` in the run.
//!
//! # Which decisions claim a governed action
//!
//! The rule, spelled out, because the two cases it separates are not separable
//! by inspection:
//!
//! **A decision is judged as a governed-action claim only when something
//! positively identifies it as one.** Two things can:
//!
//! 1. the names listed under `choice.arguments.trajectory_actions`. That key
//!    means "the governed actions this code called", so every name in it is a
//!    claim about a governed action whether or not the kernel recognises it —
//!    an envelope naming `Frobnicate` is exactly the fault to report;
//! 2. a `choice.action` that this deployment's action vocabulary contains (see
//!    [`ActionVocabulary`]) — a name the actor spec declares, a name the
//!    platform defines, or a name the kernel dispatched somewhere in this
//!    session.
//!
//! Everything else names something the kernel has no reason to believe is a
//! governed action, and is read as the agent's own tooling: skipped from the
//! walk and counted, never reported.
//!
//! # What this cannot tell apart, and what that costs
//!
//! A bare `Bash` and a bare `Frobnicate` are the same token from the same
//! producer path. Nothing in the record distinguishes a harness tool from an
//! action name the kernel has never seen — a transcript converter writes both
//! as `choice.action = <the tool the model called>` with the tool's arguments
//! beside it. So the checker cannot have both "no false violations for harness
//! tools" and "every unplaceable name reported", and this is the conservative
//! half: a name it cannot place is counted, not condemned.
//!
//! What that gives up is narrow, and it is not the governance check. Every
//! action that actually reached the platform leaves a kernel row, and rows are
//! judged in full — an undeclared action, an illegal source state, a move after
//! a terminal state are all caught there, from evidence this rule never
//! touches. What is given up is one candour signal: an agent that *reports*
//! reaching for an action which never reached the platform, whose name no spec
//! in the walk declares and which nothing in the session dispatched, is counted
//! as harness use rather than reported. The attempt failed before the kernel
//! saw it; what is lost is the record that it was made.
//!
//! The way to narrow that gap is to add positive identification, never to widen
//! what counts as a name: reading a governed target out of a decision's
//! arguments (as katagami's `scripts/trajectory/conformance_check.py` does
//! offline), or giving [`ActionVocabulary`] the tenant's other registered specs
//! so a name belonging to another actor becomes placeable. Both are additive —
//! they can only move decisions from "skipped" to "judged" — which is why the
//! vocabulary is one type with one constructor rather than a test scattered
//! through the walk.

use std::collections::BTreeSet;

use temper_ots::models::OTSDecision;
use temper_store_turso::TursoTrajectoryRow;

use super::spec_view::SpecView;
use super::{CAPTURE_LOSS_ACTION, CAPTURE_LOSS_ENTITY_TYPE, KERNEL_PLATFORM_ACTIONS};

/// Key under `choice.arguments` where a harness envelope lists the governed
/// actions the submitted code calls.
const TRAJECTORY_ACTIONS_KEY: &str = "trajectory_actions";

/// Field naming the action inside one entry of that list.
const NESTED_ACTION_KEY: &str = "action";

/// `source` on a row the kernel wrote while dispatching an action on an entity
/// (`TrajectorySource::Entity`). The serialized form, because that is what
/// reaches the checker on a stored row.
const ENTITY_DISPATCH_SOURCE: &str = "Entity";

/// What a decision names.
pub(super) enum DecisionActions<'a> {
    /// A thought rather than a callable: nothing was invoked.
    Thinking,
    /// A harness envelope — a `choice.action` that is not even shaped like an
    /// action name — which reached no governed action.
    HarnessEnvelope,
    /// A name shaped like an action name but outside this deployment's action
    /// vocabulary, so the kernel cannot read it as a governed action: the
    /// agent's own tooling, as far as the record shows.
    UnrecognizedName,
    /// Governed action names the agent decided on, in the order given.
    Actions(Vec<&'a str>),
}

/// The action names this deployment is known to define.
///
/// Three sources, all of them the kernel's own knowledge rather than the
/// agent's account of itself:
///
/// - the actor spec's declared alphabet;
/// - [`KERNEL_PLATFORM_ACTIONS`], the verbs the platform defines for every
///   actor. A decision naming one claims a platform verb, and if this actor's
///   spec does not declare it that is a
///   [`ForbiddenAction`](super::ViolationKind::ForbiddenAction) — the more
///   consequential of the two undeclared kinds, and it stays reported;
/// - every action name the kernel dispatched on an entity in this session, on
///   any entity type (see [`names_a_kernel_dispatch`], which is narrower than
///   "appears on a row"). A name the kernel dispatched is a real action name in
///   this deployment whoever it belonged to, so a decision naming it is
///   claiming a governed action. Rows on other entities widen the vocabulary
///   but never excuse a decision:
///   [`check_decisions`](super::check_decisions) still reports a name this
///   actor's spec does not declare.
///
/// # The collision this resolves against the harness
///
/// A harness tool whose name is also a platform verb — a filesystem tool called
/// `Delete`, an HTTP tool called `Put` or `Patch` — is placeable, so it is
/// judged, and against an actor that does not declare it that is a
/// `forbidden_action`. Nothing in the record separates the two readings, so one
/// of them has to be chosen, and this is the deliberate choice: the platform's
/// own write verbs are the most consequential names an agent can claim, and a
/// false report on one of them is preferable to silence on an agent reaching
/// for `Delete` or `SubmitSpec`. The tie-break runs the other way everywhere
/// else — an unplaceable name is counted, not condemned. Pinned by
/// `a_harness_tool_named_after_a_platform_verb_is_reported`.
///
/// # The agent's own account is not a source
///
/// The trajectory's own tool inventory (`context.entities` of type `tool`) is
/// deliberately not a source. A transcript converter builds it from the same
/// `choice.action` values as the decisions themselves, so it carries no
/// information the decisions do not already have — and taking the agent's word
/// for which of its decisions were "just tools" would let a trajectory relabel
/// its way out of the check.
pub(super) struct ActionVocabulary<'a> {
    names: BTreeSet<&'a str>,
}

impl<'a> ActionVocabulary<'a> {
    pub(super) fn new(spec: &SpecView<'a>, kernel_rows: &'a [TursoTrajectoryRow]) -> Self {
        let mut names: BTreeSet<&str> = spec.declared.keys().copied().collect();
        names.extend(KERNEL_PLATFORM_ACTIONS.iter().copied());
        names.extend(
            kernel_rows
                .iter()
                .filter(|row| names_a_kernel_dispatch(row))
                .map(|row| row.action.as_str()),
        );
        Self { names }
    }

    fn contains(&self, action: &str) -> bool {
        self.names.contains(action)
    }
}

/// Whether this row records the kernel dispatching an action, which is the only
/// thing that proves a name is an action at all.
///
/// Written as an allowlist of one source, because the question is whether the
/// KERNEL chose this name, and only the entity dispatch path
/// ([`TrajectorySource::Entity`](crate::state::TrajectorySource)) answers yes.
/// The other two both carry names a caller can choose:
///
/// - `Authz` rows come from `authz::helpers::record_authz_denial`, which
///   `POST /api/authorize` reaches with a caller-supplied action name, resource
///   type and `X-Temper-Ctx-SessionId`. They are written with `spec_governed`
///   unset, so testing that flag alone lets them through.
/// - `Platform` rows are kernel bookkeeping. Their names are the kernel's, but
///   they are already in [`KERNEL_PLATFORM_ACTIONS`], so admitting them adds
///   nothing — and it is what would let a capture-loss marker's `CaptureLost`
///   into the vocabulary.
///
/// `spec_governed = false` is then checked on top: `POST /api/audit` and
/// `POST /api/evolution/trajectories/unmet` write caller-named rows and mark
/// them, and nothing says such a row cannot claim `Entity` as its source.
///
/// The failure this shuts out: one `POST /api/authorize` naming action `Bash`
/// against somebody else's session id puts `Bash` in that session's vocabulary,
/// and every `Bash` in their run then reports as a violation of a spec they
/// followed. A row that cannot be placed here simply does not widen the
/// vocabulary, which is the safe direction — it can only cause a decision to be
/// counted rather than judged.
fn names_a_kernel_dispatch(row: &TursoTrajectoryRow) -> bool {
    row.source.as_deref() == Some(ENTITY_DISPATCH_SOURCE)
        && row.spec_governed != Some(false)
        && !(row.entity_type == CAPTURE_LOSS_ENTITY_TYPE && row.action == CAPTURE_LOSS_ACTION)
}

/// Read the governed actions one decision names.
pub(super) fn decision_actions<'a>(
    decision: &'a OTSDecision,
    vocabulary: &ActionVocabulary<'_>,
) -> DecisionActions<'a> {
    if !decision.decision_type.is_invocation() {
        return DecisionActions::Thinking;
    }
    let action = decision.choice.action.as_str();
    if vocabulary.contains(action) {
        return DecisionActions::Actions(vec![action]);
    }
    // Not a name this deployment defines. It may still be an envelope around
    // governed calls: those are recorded alongside it and are exactly what the
    // checker is looking for.
    let nested = nested_actions(decision);
    if !nested.is_empty() {
        return DecisionActions::Actions(nested);
    }
    if is_action_name(action) {
        DecisionActions::UnrecognizedName
    } else {
        DecisionActions::HarnessEnvelope
    }
}

/// Governed actions listed under `choice.arguments.trajectory_actions`.
fn nested_actions(decision: &OTSDecision) -> Vec<&str> {
    decision
        .choice
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get(TRAJECTORY_ACTIONS_KEY))
        .and_then(|actions| actions.as_array())
        .map(|actions| {
            actions
                .iter()
                .filter_map(|entry| entry.get(NESTED_ACTION_KEY)?.as_str())
                .filter(|action| is_action_name(action))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `action` has the shape of a declared action name.
///
/// An `[[action]]` name in an IOA spec is a bare token, so a `choice.action`
/// carrying anything else — whitespace, a colon, a newline of Python — is an
/// envelope around something that is not an action name, whatever the harness
/// that produced it.
///
/// This is a test of what a name *cannot* be, never of what it is: `Bash`
/// passes it, which is how every harness tool call came to be reported as a
/// violation. Only [`ActionVocabulary`] decides that a name is an action.
fn is_action_name(action: &str) -> bool {
    !action.is_empty()
        && action
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use temper_ots::models::{DecisionType, OTSChoice, OTSConsequence};
    use temper_spec::automaton::{Automaton, parse_automaton};

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn order_automaton() -> Automaton {
        parse_automaton(ORDER_IOA).expect("order fixture parses")
    }

    fn decision(decision_type: DecisionType, choice: OTSChoice) -> OTSDecision {
        OTSDecision::new(decision_type, choice, OTSConsequence::success())
    }

    /// Classify against the spec's own alphabet plus the platform's, with no
    /// session rows. The row-sourced half of the vocabulary is exercised
    /// end-to-end in [`super::super::conformance_test`], where a report is what
    /// the assertions are about.
    fn classify<'a>(automaton: &'a Automaton, decision: &'a OTSDecision) -> DecisionActions<'a> {
        let spec = SpecView::new(automaton);
        let vocabulary = ActionVocabulary::new(&spec, &[]);
        decision_actions(decision, &vocabulary)
    }

    fn actions<'a>(automaton: &'a Automaton, decision: &'a OTSDecision) -> Vec<&'a str> {
        match classify(automaton, decision) {
            DecisionActions::Actions(actions) => actions,
            DecisionActions::Thinking => panic!("expected actions, got a thought"),
            DecisionActions::HarnessEnvelope => panic!("expected actions, got a harness envelope"),
            DecisionActions::UnrecognizedName => {
                panic!("expected actions, got an unplaceable name")
            }
        }
    }

    #[test]
    fn a_declared_action_name_is_the_action() {
        let automaton = order_automaton();
        let decision = decision(DecisionType::ToolSelection, OTSChoice::new("ConfirmOrder"));
        assert_eq!(actions(&automaton, &decision), vec!["ConfirmOrder"]);
    }

    #[test]
    fn a_platform_verb_is_a_name_the_kernel_can_place() {
        // Not declared by `Order`, but defined by the platform: the claim is
        // judgeable, and judging it is what makes it a `forbidden_action`.
        let automaton = order_automaton();
        let decision = decision(DecisionType::ToolSelection, OTSChoice::new("Delete"));
        assert_eq!(actions(&automaton, &decision), vec!["Delete"]);
    }

    #[test]
    fn a_harness_tool_name_is_not_an_action() {
        let automaton = order_automaton();
        for tool in [
            "Bash",
            "Read",
            "Write",
            "Edit",
            "Agent",
            "ToolSearch",
            "SendMessage",
            "mcp__linear__list_issues",
        ] {
            let decision = decision(DecisionType::ToolSelection, OTSChoice::new(tool));
            assert!(
                matches!(
                    classify(&automaton, &decision),
                    DecisionActions::UnrecognizedName
                ),
                "`{tool}` is the harness's tool, not an action this deployment defines"
            );
        }
    }

    #[test]
    fn a_thought_names_nothing() {
        let automaton = order_automaton();
        let decision = decision(
            DecisionType::ReasoningStep,
            OTSChoice::new("compare shipping options"),
        );
        assert!(matches!(
            classify(&automaton, &decision),
            DecisionActions::Thinking
        ));
    }

    #[test]
    fn an_execute_envelope_yields_the_actions_the_code_called() {
        let automaton = order_automaton();
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: temper.action('default', 'Order', 'ConfirmOrder', {})")
                .with_arguments(json!({
                    "trajectory_actions": [
                        { "action": "ConfirmOrder", "params": {} },
                        { "action": "ShipOrder", "params": {} },
                    ],
                })),
        );
        assert_eq!(
            actions(&automaton, &decision),
            vec!["ConfirmOrder", "ShipOrder"]
        );
    }

    #[test]
    fn an_envelope_names_its_governed_actions_even_when_the_kernel_cannot_place_them() {
        // The envelope's own key says these were governed calls, so an
        // undeclared one is reported rather than passed over as tooling. This
        // is the positive identification a bare name does not have.
        let automaton = order_automaton();
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: temper.action('default', 'Order', 'Frobnicate', {})")
                .with_arguments(json!({
                    "trajectory_actions": [{ "action": "Frobnicate", "params": {} }],
                })),
        );
        assert_eq!(actions(&automaton, &decision), vec!["Frobnicate"]);
    }

    #[test]
    fn an_execute_envelope_that_called_nothing_governed_is_a_harness_envelope() {
        let automaton = order_automaton();
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: print('hello')"),
        );
        assert!(
            matches!(
                classify(&automaton, &decision),
                DecisionActions::HarnessEnvelope
            ),
            "a code envelope names no governed action, so it must not be judged as one"
        );
    }

    #[test]
    fn a_nested_list_of_non_names_is_a_harness_envelope() {
        let automaton = order_automaton();
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: temper.action(...)").with_arguments(json!({
                "trajectory_actions": [{ "action": "not an action name" }],
            })),
        );
        assert!(matches!(
            classify(&automaton, &decision),
            DecisionActions::HarnessEnvelope
        ));
    }
}
