//! TransitionTable micro-benchmarks.
//!
//! Measures the hot-path evaluation performance of the JIT transition table
//! using the real Order I/O Automaton specification.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use temper_jit::table::types::{EvalContext, TransitionTable};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> TransitionTable {
    TransitionTable::from_ioa_source(ORDER_IOA)
}

fn ctx_with_items(n: usize) -> EvalContext {
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".to_string(), n);
    ctx
}

fn bench_evaluate_ctx(c: &mut Criterion) {
    let table = order_table();
    let mut group = c.benchmark_group("evaluate_ctx");

    // Successful transition: SubmitOrder from Draft with items=2
    group.bench_function("hit", |b| {
        let ctx = ctx_with_items(2);
        b.iter(|| {
            black_box(table.evaluate_ctx(
                black_box("Draft"),
                black_box(&ctx),
                black_box("SubmitOrder"),
            ))
        })
    });

    // Guard failure: SubmitOrder from Draft with items=0
    group.bench_function("guard_fail", |b| {
        let ctx = ctx_with_items(0);
        b.iter(|| {
            black_box(table.evaluate_ctx(
                black_box("Draft"),
                black_box(&ctx),
                black_box("SubmitOrder"),
            ))
        })
    });

    // Wrong state: SubmitOrder from Shipped
    group.bench_function("state_miss", |b| {
        let ctx = ctx_with_items(2);
        b.iter(|| {
            black_box(table.evaluate_ctx(
                black_box("Shipped"),
                black_box(&ctx),
                black_box("SubmitOrder"),
            ))
        })
    });

    // Unknown action name
    group.bench_function("unknown_action", |b| {
        let ctx = ctx_with_items(0);
        b.iter(|| {
            black_box(table.evaluate_ctx(
                black_box("Draft"),
                black_box(&ctx),
                black_box("NonExistentAction"),
            ))
        })
    });

    // Average across all non-output actions
    group.bench_function("all_actions", |b| {
        let actions = [
            ("Draft", "AddItem", 0),
            ("Draft", "RemoveItem", 2),
            ("Draft", "SubmitOrder", 2),
            ("Submitted", "ConfirmOrder", 1),
            ("Confirmed", "ProcessOrder", 1),
            ("Processing", "ShipOrder", 1),
            ("Shipped", "DeliverOrder", 1),
            ("Draft", "CancelOrder", 0),
            ("Shipped", "InitiateReturn", 1),
            ("ReturnRequested", "CompleteReturn", 1),
        ];
        b.iter(|| {
            for &(state, action, items) in &actions {
                let ctx = ctx_with_items(items);
                black_box(table.evaluate_ctx(black_box(state), black_box(&ctx), black_box(action)));
            }
        })
    });

    group.finish();
}

fn bench_table_construction(c: &mut Criterion) {
    c.bench_function("table_construction", |b| {
        b.iter(|| black_box(TransitionTable::from_ioa_source(black_box(ORDER_IOA))))
    });
}

fn bench_eval_context_construction(c: &mut Criterion) {
    c.bench_function("eval_context_construction", |b| {
        b.iter(|| {
            let mut ctx = EvalContext::default();
            ctx.counters.insert("items".to_string(), 2);
            ctx.counters.insert("review_cycles".to_string(), 1);
            ctx.booleans.insert("has_address".to_string(), true);
            black_box(ctx)
        })
    });
}

fn bench_rebuild_index(c: &mut Criterion) {
    let mut table = order_table();
    c.bench_function("rebuild_index", |b| {
        b.iter(|| {
            table.rebuild_index();
            black_box(&table);
        })
    });
}

criterion_group!(
    benches,
    bench_evaluate_ctx,
    bench_table_construction,
    bench_eval_context_construction,
    bench_rebuild_index,
);
criterion_main!(benches);
