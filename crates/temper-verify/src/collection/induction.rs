//! Symbolic induction for roster sizes beyond exact labelled exploration.

use temper_spec::automaton::CollectionWorkflow;
use z3::ast::{Bool, Int};
use z3::{SatResult, Solver};

pub(super) fn prove_transition_induction(
    workflow: &CollectionWorkflow,
    members: u16,
) -> Result<usize, String> {
    let schemas = 5usize + usize::from(workflow.max_attempts) * 4;
    for schema in 0..schemas {
        prove_schema(workflow, members, schema)?;
    }
    Ok(schemas)
}

fn prove_schema(workflow: &CollectionWorkflow, members: u16, schema: usize) -> Result<(), String> {
    let solver = Solver::new();
    let names = [
        "pending",
        "succeeded",
        "failed",
        "cancelled",
        "timed_out",
        "join",
        "phase",
        "class",
        "roster",
        "identity",
    ];
    let pre = names
        .iter()
        .map(|n| Int::new_const(format!("{n}_pre")))
        .collect::<Vec<_>>();
    let attempts = (0..workflow.max_attempts)
        .map(|i| Int::new_const(format!("attempt_{i}_pre")))
        .collect::<Vec<_>>();
    let mut post = pre.clone();
    let mut post_attempts = attempts.clone();
    constrain_invariant(&solver, workflow, members, &pre, &attempts);
    let one = Int::from_i64(1);
    let zero = Int::from_i64(0);
    let bucket_schemas = usize::from(workflow.max_attempts) * 4;
    if schema < bucket_schemas {
        let bucket = schema / 4;
        let outcome = schema % 4;
        solver.assert(attempts[bucket].gt(&zero));
        post_attempts[bucket] = &attempts[bucket] - &one;
        match outcome {
            0 => post[1] = &pre[1] + &one,
            1 => post[2] = &pre[2] + &one,
            2 if bucket + 1 < attempts.len() => {
                solver.assert(pre[6].eq(&zero));
                post_attempts[bucket + 1] = &attempts[bucket + 1] + &one;
            }
            2 => solver.assert(Bool::from_bool(false)),
            3 => {
                solver.assert(Bool::or(&[
                    &pre[6].eq(Int::from_i64(1)),
                    &pre[6].eq(Int::from_i64(2)),
                ]));
                post[3] = pre[6].eq(Int::from_i64(1)).ite(&(&pre[3] + &one), &pre[3]);
                post[4] = pre[6].eq(Int::from_i64(2)).ite(&(&pre[4] + &one), &pre[4]);
            }
            _ => unreachable!(),
        }
    } else {
        match schema - bucket_schemas {
            0 => {
                solver.assert(pre[6].eq(&zero));
                solver.assert(pre[0].gt(&zero));
                solver
                    .assert(sum(&attempts).lt(Int::from_i64(i64::from(workflow.max_concurrency))));
                post[0] = &pre[0] - &one;
                post_attempts[0] = &attempts[0] + &one;
            }
            1 | 2 => {
                solver.assert(pre[6].eq(&zero));
                solver.assert(pre[5].eq(&zero));
                let phase = i64::try_from(schema - bucket_schemas).unwrap();
                post[6] = Int::from_i64(phase);
                post[0] = zero.clone();
                let target = if phase == 1 { 3 } else { 4 };
                post[target] = &pre[target] + &pre[0];
            }
            3 => {
                solver.assert(pre[5].eq(&zero));
                solver.assert(
                    (&pre[1] + &pre[2] + &pre[3] + &pre[4]).eq(Int::from_i64(i64::from(members))),
                );
                post[5] = one.clone();
                post[7] = classification_expr(&pre, members);
            }
            4 => {}
            _ => unreachable!(),
        }
    }
    solver.assert(invariant_violation(
        workflow,
        members,
        &post,
        &post_attempts,
    ));
    if !matches!(solver.check(), SatResult::Unsat) {
        return Err(format!(
            "collection workflow '{}' transition schema {schema} is not inductive",
            workflow.name
        ));
    }
    Ok(())
}

fn sum(values: &[Int]) -> Int {
    Int::add(&values.iter().collect::<Vec<_>>())
}

fn classification_expr(state: &[Int], members: u16) -> Int {
    let failed = Int::from_i64(2);
    state[6].eq(Int::from_i64(2)).ite(
        &Int::from_i64(4),
        &state[6].eq(Int::from_i64(1)).ite(
            &Int::from_i64(3),
            &state[1].eq(Int::from_i64(i64::from(members))).ite(
                &Int::from_i64(0),
                &state[1]
                    .gt(Int::from_i64(0))
                    .ite(&Int::from_i64(1), &failed),
            ),
        ),
    )
}

fn constrain_invariant(
    solver: &Solver,
    workflow: &CollectionWorkflow,
    members: u16,
    state: &[Int],
    attempts: &[Int],
) {
    solver.assert(invariant_violation(workflow, members, state, attempts).not());
}

fn invariant_violation(
    workflow: &CollectionWorkflow,
    members: u16,
    state: &[Int],
    attempts: &[Int],
) -> Bool {
    let zero = Int::from_i64(0);
    let one = Int::from_i64(1);
    let attempts_sum = sum(attempts);
    let terminal_sum = &state[1] + &state[2] + &state[3] + &state[4];
    let total = &state[0] + &attempts_sum + &terminal_sum;
    let mut violations = state[0..5]
        .iter()
        .chain(attempts)
        .map(|value| value.lt(&zero))
        .collect::<Vec<_>>();
    violations.extend([
        total.ne(Int::from_i64(i64::from(members))),
        attempts_sum.gt(Int::from_i64(i64::from(workflow.max_concurrency))),
        Bool::or(&[&state[6].lt(&zero), &state[6].gt(Int::from_i64(2))]),
        Bool::and(&[&state[6].ne(&zero), &state[0].ne(&zero)]),
        Bool::or(&[&state[5].lt(&zero), &state[5].gt(&one)]),
        state[8].ne(Int::from_i64(i64::from(members))),
        state[9].ne(&zero),
        Bool::and(&[&state[5].eq(&zero), &state[7].ne(Int::from_i64(-1))]),
        Bool::and(&[
            &state[5].eq(&one),
            &Bool::or(&[
                &terminal_sum.ne(Int::from_i64(i64::from(members))),
                &state[7].ne(classification_expr(state, members)),
            ]),
        ]),
    ]);
    Bool::or(&violations.iter().collect::<Vec<_>>())
}
