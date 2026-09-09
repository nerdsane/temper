//! What one OTS decision contributes to the action walk.
//!
//! A decision's `choice.action` is not always an action name. Three shapes
//! reach the checker:
//!
//! - a thought — a reasoning step or a response formulation, whose `action` is
//!   free-form prose ([`temper_ots::models::DecisionType::is_invocation`]);
//! - a **harness envelope** — an invocation of the harness's own tool rather
//!   than of a governed action. The MCP server records every `execute` tool
//!   call as one decision whose `choice.action` is `"execute: <the submitted
//!   code>"` (`temper_mcp::runtime::record_execute_turn`), with the governed
//!   actions the code calls listed under `choice.arguments.trajectory_actions`;
//! - an action name, which is what the checker can judge.
//!
//! Reading the first two as action names reports attempts the agent never made:
//! every MCP-produced decision would surface as one `unknown_action` violation
//! naming a hundred characters of Python.

use temper_ots::models::OTSDecision;

/// Key under `choice.arguments` where a harness envelope lists the governed
/// actions the submitted code calls.
const TRAJECTORY_ACTIONS_KEY: &str = "trajectory_actions";

/// Field naming the action inside one entry of that list.
const NESTED_ACTION_KEY: &str = "action";

/// What a decision names.
pub(super) enum DecisionActions<'a> {
    /// A thought rather than a callable: nothing was invoked.
    Thinking,
    /// A harness tool, naming no governed action the checker can judge.
    HarnessTool,
    /// Governed action names the agent decided on, in the order given.
    Actions(Vec<&'a str>),
}

/// Read the governed actions one decision names.
pub(super) fn decision_actions(decision: &OTSDecision) -> DecisionActions<'_> {
    if !decision.decision_type.is_invocation() {
        return DecisionActions::Thinking;
    }
    let action = decision.choice.action.as_str();
    if is_action_name(action) {
        return DecisionActions::Actions(vec![action]);
    }
    // The envelope itself names no governed action, but the actions the code
    // inside it calls are recorded alongside it and are exactly what the
    // checker is looking for.
    let nested = nested_actions(decision);
    if nested.is_empty() {
        DecisionActions::HarnessTool
    } else {
        DecisionActions::Actions(nested)
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
/// An `[[action]]` name in an IOA spec is a bare token. A `choice.action`
/// carrying anything else — whitespace, a colon, a newline of Python — is a
/// harness envelope around something that is not an action name, whatever the
/// harness that produced it.
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

    fn decision(decision_type: DecisionType, choice: OTSChoice) -> OTSDecision {
        OTSDecision::new(decision_type, choice, OTSConsequence::success())
    }

    fn actions(decision: &OTSDecision) -> Vec<&str> {
        match decision_actions(decision) {
            DecisionActions::Actions(actions) => actions,
            DecisionActions::Thinking => panic!("expected actions, got a thought"),
            DecisionActions::HarnessTool => panic!("expected actions, got a harness tool"),
        }
    }

    #[test]
    fn a_bare_action_name_is_the_action() {
        let decision = decision(DecisionType::ToolSelection, OTSChoice::new("ConfirmOrder"));
        assert_eq!(actions(&decision), vec!["ConfirmOrder"]);
    }

    #[test]
    fn a_thought_names_nothing() {
        let decision = decision(
            DecisionType::ReasoningStep,
            OTSChoice::new("compare shipping options"),
        );
        assert!(matches!(
            decision_actions(&decision),
            DecisionActions::Thinking
        ));
    }

    #[test]
    fn an_execute_envelope_yields_the_actions_the_code_called() {
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
        assert_eq!(actions(&decision), vec!["ConfirmOrder", "ShipOrder"]);
    }

    #[test]
    fn an_execute_envelope_that_called_nothing_governed_is_a_harness_tool() {
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: print('hello')"),
        );
        assert!(
            matches!(decision_actions(&decision), DecisionActions::HarnessTool),
            "a code envelope names no governed action, so it must not be judged as one"
        );
    }

    #[test]
    fn a_nested_list_of_non_names_is_a_harness_tool() {
        let decision = decision(
            DecisionType::ToolSelection,
            OTSChoice::new("execute: temper.action(...)").with_arguments(json!({
                "trajectory_actions": [{ "action": "not an action name" }],
            })),
        );
        assert!(matches!(
            decision_actions(&decision),
            DecisionActions::HarnessTool
        ));
    }
}
