//! Server-level actor throughput benchmarks.
//!
//! Measures raw actor dispatch overhead (spawn + message + evaluate).
//! For realistic e-commerce agent benchmarks, see reference-apps/ecommerce/.
//!
//! Run with: `cargo bench -p temper-server --bench actor_throughput`

use std::collections::BTreeMap;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use temper_runtime::ActorSystem;
use temper_server::ServerState;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn build_state(name: &str) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut ioa_sources = BTreeMap::new();
    ioa_sources.insert("Order".to_string(), ORDER_IOA.to_string());
    let system = ActorSystem::new(name);
    ServerState::with_specs(system, csdl, CSDL_XML.to_string(), ioa_sources).unwrap()
}

#[allow(deprecated)] // Legacy dispatch_action used intentionally in benchmarks
fn bench_actor(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("actor");

    // Single action: spawn actor + dispatch AddItem
    {
        let state = build_state("bench-single");
        let mut id = 0u64;
        group.bench_function("single_action", |b| {
            b.iter(|| {
                id += 1;
                rt.block_on(async {
                    black_box(
                        state
                            .dispatch_action(
                                "Order",
                                &format!("o-{id}"),
                                "AddItem",
                                serde_json::json!({"ProductId": "p1"}),
                            )
                            .await
                            .unwrap(),
                    )
                })
            })
        });
    }

    // Concurrent: 10 / 100 / 1000 entities, 1 action each
    for count in [10, 100, 1000] {
        let state = build_state(&format!("bench-conc-{count}"));
        let mut batch = 0u64;
        group.bench_function(format!("concurrent_{count}"), |b| {
            b.iter(|| {
                batch += 1;
                rt.block_on(async {
                    let mut handles = Vec::with_capacity(count);
                    for i in 0..count {
                        let s = state.clone();
                        let eid = format!("o-{batch}-{i}");
                        handles.push(tokio::spawn(async move { // determinism-ok: benchmark concurrency, not simulation
                            s.dispatch_action(
                                "Order",
                                &eid,
                                "AddItem",
                                serde_json::json!({"ProductId": "p1"}),
                            )
                            .await
                            .unwrap()
                        }));
                    }
                    for h in handles {
                        black_box(h.await.unwrap());
                    }
                })
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_actor);
criterion_main!(benches);
