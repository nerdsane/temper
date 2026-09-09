use crate::error::ODataError;

/// Resource budgets for one `$filter` expression.
///
/// These budgets bound every input-controlled dimension before the parser can
/// build an AST large enough to exhaust memory or the request thread's stack.
pub(super) const FILTER_INPUT_BYTE_BUDGET: usize = 64 * 1024;
pub(super) const FILTER_TOKEN_BUDGET: usize = 4_096;
pub(super) const FILTER_NODE_BUDGET: usize = 4_096;
pub(super) const FILTER_OPERATOR_BUDGET: usize = 1_024;
/// Total function-call arguments across the whole filter (a cumulative, not
/// per-call, budget). It bounds total argument-parsing work regardless of how the
/// calls are distributed; generous for real queries, which use only a handful of
/// function calls.
pub(super) const FILTER_ARGUMENT_BUDGET: usize = 256;
pub(super) const FILTER_LITERAL_BYTE_BUDGET: usize = 16 * 1024;

#[derive(Debug)]
pub(super) struct FilterBudget {
    tokens_remaining: usize,
    nodes_remaining: usize,
    operators_remaining: usize,
    arguments_remaining: usize,
    literal_bytes_remaining: usize,
}

impl FilterBudget {
    pub(super) fn new(input: &str) -> Result<Self, ODataError> {
        if input.len() > FILTER_INPUT_BYTE_BUDGET {
            // Report the actual over-budget length as the position rather than the
            // fixed budget, so the error says how large the rejected input was.
            return Err(budget_exceeded(
                "input byte",
                FILTER_INPUT_BYTE_BUDGET,
                input.len(),
            ));
        }

        Ok(Self {
            tokens_remaining: FILTER_TOKEN_BUDGET,
            nodes_remaining: FILTER_NODE_BUDGET,
            operators_remaining: FILTER_OPERATOR_BUDGET,
            arguments_remaining: FILTER_ARGUMENT_BUDGET,
            literal_bytes_remaining: FILTER_LITERAL_BYTE_BUDGET,
        })
    }

    pub(super) fn consume_token(&mut self, position: usize) -> Result<(), ODataError> {
        consume_budget(
            &mut self.tokens_remaining,
            1,
            "token",
            FILTER_TOKEN_BUDGET,
            position,
        )
    }

    pub(super) fn consume_node(&mut self, position: usize) -> Result<(), ODataError> {
        consume_budget(
            &mut self.nodes_remaining,
            1,
            "AST node",
            FILTER_NODE_BUDGET,
            position,
        )
    }

    pub(super) fn consume_operator(&mut self, position: usize) -> Result<(), ODataError> {
        consume_budget(
            &mut self.operators_remaining,
            1,
            "operator",
            FILTER_OPERATOR_BUDGET,
            position,
        )
    }

    pub(super) fn consume_argument(&mut self, position: usize) -> Result<(), ODataError> {
        consume_budget(
            &mut self.arguments_remaining,
            1,
            "function argument",
            FILTER_ARGUMENT_BUDGET,
            position,
        )
    }

    pub(super) fn consume_literal_bytes(
        &mut self,
        amount: usize,
        position: usize,
    ) -> Result<(), ODataError> {
        consume_budget(
            &mut self.literal_bytes_remaining,
            amount,
            "string literal byte",
            FILTER_LITERAL_BYTE_BUDGET,
            position,
        )
    }
}

fn consume_budget(
    remaining: &mut usize,
    amount: usize,
    resource: &str,
    allowance: usize,
    position: usize,
) -> Result<(), ODataError> {
    let Some(next) = remaining.checked_sub(amount) else {
        return Err(budget_exceeded(resource, allowance, position));
    };
    *remaining = next;
    Ok(())
}

fn budget_exceeded(resource: &str, allowance: usize, position: usize) -> ODataError {
    ODataError::InvalidFilter {
        message: format!("filter {resource} budget of {allowance} exceeded"),
        position,
    }
}
