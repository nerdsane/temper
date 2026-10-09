//! Independently owned roots can discover a cycle after both effects start.
use super::*;
use async_trait::async_trait;
use std::sync::{Arc, atomic::AtomicUsize};
use temper_server::StorageStack;
use temper_server::adapters::{
    AdapterContext, AdapterError, AdapterRegistry, AdapterResult, AgentAdapter,
};
use temper_store_sim::SimEventStore;
use tokio::sync::{Notify, Semaphore};

struct EffectGate {
    started: Notify,
    release: Semaphore,
    calls: AtomicUsize,
}
#[async_trait]
impl AgentAdapter for EffectGate {
    fn adapter_type(&self) -> &str {
        "cycle-gate"
    }
    async fn execute(&self, _: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(AdapterResult::success(serde_json::json!({}), 0))
    }
}

#[tokio::test(start_paused = true)]
async fn independently_started_cycle_roots_do_not_wait_on_each_others_terminal() {
    let (_guard, _, _) = install_deterministic_context(51980);
    let adapter_trigger =
        "\n[[action.triggers]]\nname=\"gate\"\nkind=\"adapter\"\nadapter=\"cycle-gate\"\n";
    let source = reaction_spec("Source", &["Child"]) + adapter_trigger;
    let child = reaction_spec("Child", &["Source", "Leaf"]) + adapter_trigger;
    let leaf = reaction_spec("Leaf", &[]);
    let mut state = state_with_specs(&[("Source", &source), ("Child", &child), ("Leaf", &leaf)]);
    let store = SimEventStore::no_faults(51980);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let gate = Arc::new(EffectGate {
        started: Notify::new(),
        release: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let mut adapters = AdapterRegistry::new();
    adapters.register(gate.clone());
    state.adapter_registry = Arc::new(adapters);
    let mut source = Box::pin(run(&state, "Source", true, true));
    tokio::select! { biased;
        result = &mut source => panic!("source returned before owned effects: {result:?}"),
        _ = gate.started.notified() => {}
    }
    let mut child = Box::pin(run(&state, "Child", true, true));
    tokio::select! { biased;
        result = &mut child => panic!("child returned before owned effects: {result:?}"),
        _ = gate.started.notified() => {}
    }
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        2,
        "both independent effects owners must exist before reactions start"
    );
    gate.release.add_permits(2);
    let (child, source) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(child, source)
    })
    .await
    .expect("independent receipt owners formed a reaction-terminal wait cycle");
    assert!(child.success && source.success);
    for kind in ["Source", "Child", "Leaf"] {
        assert_eq!(action_count(&store, kind, "Start"), 1);
    }
}

async fn two_owned_roots_keep_remaining_obligations(cycle: bool, seed: u64) {
    let (_guard, _, _) = install_deterministic_context(seed);
    let adapter_trigger =
        "\n[[action.triggers]]\nname=\"gate\"\nkind=\"adapter\"\nadapter=\"cycle-gate\"\n";
    let source = reaction_spec("Source", &["Child", "SourceLeaf"]) + adapter_trigger;
    let child = reaction_spec(
        "Child",
        if cycle {
            &["Source", "ChildLeaf"]
        } else {
            &["ChildLeaf"]
        },
    ) + adapter_trigger;
    let source_leaf = reaction_spec("SourceLeaf", &[]);
    let child_leaf = reaction_spec("ChildLeaf", &[]);
    let mut state = state_with_specs(&[
        ("Source", &source),
        ("Child", &child),
        ("SourceLeaf", &source_leaf),
        ("ChildLeaf", &child_leaf),
    ]);
    let store = SimEventStore::no_faults(seed);
    let leaves = gate_commits(&mut state, store.clone(), &["SourceLeaf", "ChildLeaf"]);
    let gate = Arc::new(EffectGate {
        started: Notify::new(),
        release: Semaphore::new(0),
        calls: AtomicUsize::new(0),
    });
    let mut adapters = AdapterRegistry::new();
    adapters.register(gate.clone());
    state.adapter_registry = Arc::new(adapters);
    let mut source = Box::pin(run(&state, "Source", true, true));
    tokio::select! { biased;
        reply = &mut source => panic!("source effects returned early: {reply:?}"),
        _ = gate.started.notified() => {}
    }
    let mut child = Box::pin(run(&state, "Child", true, true));
    tokio::select! { biased;
        reply = &mut child => panic!("child effects returned early: {reply:?}"),
        _ = gate.started.notified() => {}
    }
    let mut source_waiter = Box::pin(run(&state, "Source", true, true));
    let mut child_waiter = Box::pin(run(&state, "Child", true, true));
    pending(&mut source_waiter).await;
    pending(&mut child_waiter).await;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
    gate.release.add_permits(2);
    let first_source = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::select! {
            _ = leaves["SourceLeaf"].entered.notified() => true,
            _ = leaves["ChildLeaf"].entered.notified() => false,
        }
    })
    .await
    .expect("cross-edge prevented both remaining rules");
    pending(&mut source_waiter).await;
    pending(&mut child_waiter).await;
    assert_eq!(action_count(&store, "SourceLeaf", "Start"), 0);
    assert_eq!(action_count(&store, "ChildLeaf", "Start"), 0);
    let (first_leaf, second_leaf) = if first_source {
        ("SourceLeaf", "ChildLeaf")
    } else {
        ("ChildLeaf", "SourceLeaf")
    };
    if !cycle {
        assert!(!first_source, "acyclic parent bypassed its child terminal");
    }
    leaves[first_leaf].release.add_permits(1);
    tokio::time::timeout(
        Duration::from_secs(1),
        leaves[second_leaf].entered.notified(),
    )
    .await
    .unwrap();
    // Cutting a cycle never marks the other receipt terminal. Its independent
    // observer still waits for its own remaining rule after the cross-edge.
    if first_source {
        pending(&mut child_waiter).await;
    } else {
        pending(&mut source_waiter).await;
    }
    assert_eq!(action_count(&store, first_leaf, "Start"), 1);
    assert_eq!(action_count(&store, second_leaf, "Start"), 0);
    leaves[second_leaf].release.add_permits(1);
    let (source, child, source_waiter, child_waiter) =
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(source, child, source_waiter, child_waiter)
        })
        .await
        .unwrap();
    assert!(source.success && child.success);
    assert_eq!(
        serde_json::to_value(source).unwrap(),
        serde_json::to_value(source_waiter).unwrap()
    );
    assert_eq!(
        serde_json::to_value(child).unwrap(),
        serde_json::to_value(child_waiter).unwrap()
    );
    for kind in ["Source", "Child", "SourceLeaf", "ChildLeaf"] {
        assert_eq!(action_count(&store, kind, "Start"), 1);
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn cyclic_roots_and_independent_waiters_keep_each_remaining_rule() {
    two_owned_roots_keep_remaining_obligations(true, 51981).await;
}

#[tokio::test(start_paused = true)]
async fn acyclic_concurrent_root_dependency_still_joins_its_child_terminal() {
    two_owned_roots_keep_remaining_obligations(false, 51982).await;
}
