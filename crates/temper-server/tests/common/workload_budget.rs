//! Bounded-work accounting and visible progress for randomized DST workloads.

use std::io::Write;

use crate::common::workload_gen::WorkloadOp;

/// Consumable upper bounds for one randomized workload.
#[derive(Debug)]
pub struct WorkloadBudget {
    operations_remaining: usize,
    installs_remaining: usize,
    invariant_sweeps_remaining: usize,
}

impl WorkloadBudget {
    /// Create a budget for one workload run.
    pub fn new(operations: usize, installs: usize, check_invariants_inline: bool) -> Self {
        Self {
            operations_remaining: operations,
            installs_remaining: installs,
            invariant_sweeps_remaining: if check_invariants_inline {
                operations
            } else {
                0
            },
        }
    }

    /// Consume the budget for one generated operation.
    pub fn consume_operation(&mut self, seed: u64, op_idx: usize, op: &WorkloadOp) {
        self.operations_remaining = self.operations_remaining.checked_sub(1).unwrap_or_else(|| {
            panic!("seed {seed}, op {op_idx}: operation budget exhausted before {op:?}")
        });
        if matches!(op, WorkloadOp::InstallApp { .. }) {
            self.installs_remaining = self.installs_remaining.checked_sub(1).unwrap_or_else(|| {
                panic!("seed {seed}, op {op_idx}: install budget exhausted before {op:?}")
            });
        }
    }

    /// Consume one post-operation invariant sweep.
    pub fn consume_invariant_sweep(&mut self, seed: u64, op_idx: usize, op: &WorkloadOp) {
        self.invariant_sweeps_remaining = self
            .invariant_sweeps_remaining
            .checked_sub(1)
            .unwrap_or_else(|| {
                panic!("seed {seed}, op {op_idx}: invariant-sweep budget exhausted after {op:?}")
            });
    }

    /// Assert that every mandatory budget was consumed exactly.
    pub fn assert_consumed(&self, seed: u64) {
        assert_eq!(
            self.operations_remaining, 0,
            "seed {seed}: workload did not consume its operation budget"
        );
        assert_eq!(
            self.invariant_sweeps_remaining, 0,
            "seed {seed}: workload did not consume its invariant-sweep budget"
        );
    }
}

/// Return the stable diagnostic name for an operation variant.
pub fn operation_kind(op: &WorkloadOp) -> &'static str {
    match op {
        WorkloadOp::InstallApp { .. } => "install-app",
        WorkloadOp::Dispatch { .. } => "dispatch",
        WorkloadOp::Restart => "restart",
        WorkloadOp::CheckInvariants => "check-invariants",
    }
}

/// Format a stable operation-phase progress record.
pub fn format_operation_progress(
    scenario: &str,
    seed: u64,
    op_idx: usize,
    num_ops: usize,
    op: &WorkloadOp,
    phase: &str,
) -> String {
    format!(
        "DST progress scenario={scenario} seed={seed} operation={}/{num_ops} kind={} phase={phase}",
        op_idx + 1,
        operation_kind(op),
    )
}

/// Write progress directly so libtest capture cannot hide a stalled phase.
pub fn report_progress(message: &str) {
    let write_result = writeln!(std::io::stderr().lock(), "{message}");
    assert!(write_result.is_ok(), "failed to write DST progress");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install_op() -> WorkloadOp {
        WorkloadOp::InstallApp {
            tenant: "t-alpha".to_string(),
            app: "project-management".to_string(),
        }
    }

    #[test]
    fn exact_inline_budget_is_consumed() {
        let op = install_op();
        let mut budget = WorkloadBudget::new(1, 1, true);
        budget.consume_operation(7, 0, &op);
        budget.consume_invariant_sweep(7, 0, &op);
        budget.assert_consumed(7);
    }

    #[test]
    fn disabled_inline_checks_need_no_invariant_budget() {
        let op = WorkloadOp::Restart;
        let mut budget = WorkloadBudget::new(1, 0, false);
        budget.consume_operation(7, 0, &op);
        budget.assert_consumed(7);
    }

    #[test]
    #[should_panic(expected = "operation budget exhausted")]
    fn operation_budget_underflow_fails_fast() {
        WorkloadBudget::new(0, 0, false).consume_operation(7, 3, &WorkloadOp::Restart);
    }

    #[test]
    #[should_panic(expected = "install budget exhausted")]
    fn install_budget_underflow_fails_fast() {
        WorkloadBudget::new(1, 0, false).consume_operation(7, 3, &install_op());
    }

    #[test]
    #[should_panic(expected = "invariant-sweep budget exhausted")]
    fn invariant_budget_underflow_fails_fast() {
        WorkloadBudget::new(1, 0, false).consume_invariant_sweep(
            7,
            3,
            &WorkloadOp::CheckInvariants,
        );
    }

    #[test]
    #[should_panic(expected = "did not consume its operation budget")]
    fn incomplete_operation_budget_fails_postcondition() {
        WorkloadBudget::new(1, 0, false).assert_consumed(7);
    }

    #[test]
    #[should_panic(expected = "did not consume its invariant-sweep budget")]
    fn incomplete_invariant_budget_fails_postcondition() {
        let mut budget = WorkloadBudget::new(1, 0, true);
        budget.consume_operation(7, 0, &WorkloadOp::Restart);
        budget.assert_consumed(7);
    }

    #[test]
    fn operation_kinds_and_progress_format_are_stable() {
        let dispatch = WorkloadOp::Dispatch {
            tenant: "t-alpha".to_string(),
            entity_type: "Issue".to_string(),
            entity_id: "e-1".to_string(),
            action: "Archive".to_string(),
        };
        assert_eq!(operation_kind(&install_op()), "install-app");
        assert_eq!(operation_kind(&dispatch), "dispatch");
        assert_eq!(operation_kind(&WorkloadOp::Restart), "restart");
        assert_eq!(
            operation_kind(&WorkloadOp::CheckInvariants),
            "check-invariants"
        );
        assert_eq!(
            format_operation_progress("no-faults", 7, 2, 20, &dispatch, "execute"),
            "DST progress scenario=no-faults seed=7 operation=3/20 kind=dispatch phase=execute"
        );
    }
}
