use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::automaton::{LivenessViolation, ResolvedEffect, dispatch_effects};

const PREPARED: &str = r#"
[automaton]
name = "Cached"
states = ["Ready", "Done"]
initial = "Ready"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
from = ["Ready"]
to = "Done"
guard = "count >= 0"

[[action.triggers]]
name = "notify"
kind = "wasm"
module = "notify.wasm"
on_success = "Complete"
on_failure = "Expire"

[[action]]
name = "Expire"
from = []
to = "Done"

[[state_timeout]]
state = "Ready"
after_seconds = 30
on_timeout = "Expire"
"#;

const TRAP: &str = r#"
[automaton]
name = "CacheLivenessTrap"
states = ["Ready", "Running", "Done"]
initial = "Ready"

[[action]]
name = "Begin"
from = ["Ready"]
to = "Running"

[[action]]
name = "Complete"
from = ["Running"]
to = "Done"
"#;

fn local_cache(entries: usize, bytes: usize) -> Mutex<PreparedAutomatonCache> {
    Mutex::new(PreparedAutomatonCache::new(entries, bytes))
}

fn parse(
    source: &str,
    mode: LivenessEnforcement,
    cache: &Mutex<PreparedAutomatonCache>,
) -> Result<Automaton, AutomatonParseError> {
    parse_with_cache(source, mode, || None, cache)
}

struct LivenessWarningCounter(Arc<AtomicUsize>);

impl tracing::Subscriber for LivenessWarningCounter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() == tracing::Level::WARN
            && metadata.target().ends_with("automaton::parser")
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if self.enabled(event.metadata()) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn hits_preserve_complete_prepared_asts_and_return_independent_owned_clones() {
    let cache = local_cache(8, TOTAL_SOURCE_BYTE_BUDGET);
    for source in [
        PREPARED,
        include_str!("../../../../../test-fixtures/specs/order.ioa.toml"),
        include_str!("../../../../../os-apps/project-management/specs/issue.ioa.toml"),
    ] {
        let fresh = prepare_automaton(source).unwrap();
        let mut first = parse(source, LivenessEnforcement::WarnOnly, &cache).unwrap();
        let snapshot = cache.lock().unwrap().get(source).unwrap();
        let second = parse(source, LivenessEnforcement::WarnOnly, &cache).unwrap();
        assert_eq!(
            serde_json::to_value(&fresh).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&fresh).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        for (expected, actual) in fresh.actions.iter().zip(&second.actions) {
            assert_eq!(expected.guard, actual.guard);
            for (expected, actual) in expected.triggers.iter().zip(&actual.triggers) {
                assert_eq!(expected.guard, actual.guard);
            }
        }

        first.automaton.name = "locally changed".into();
        first.actions.clear();
        first.integrations.clear();
        assert!(Arc::ptr_eq(
            &snapshot,
            &cache.lock().unwrap().get(source).unwrap()
        ));
        let third = parse(source, LivenessEnforcement::WarnOnly, &cache).unwrap();
        assert_eq!(
            serde_json::to_value(&fresh).unwrap(),
            serde_json::to_value(&third).unwrap()
        );
    }

    let prepared = parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    assert_eq!(prepared.actions[1].from, ["Ready"]);
    assert_eq!(prepared.integrations.len(), 1);
    assert_eq!(prepared.integrations[0].name, "notify");
    assert_eq!(
        prepared.integrations[0].trigger,
        "__trigger__:Complete:notify"
    );
    assert_eq!(
        dispatch_effects(&prepared.actions[0])
            .iter()
            .filter(|effect| matches!(
                effect, ResolvedEffect::Dispatch(name) if name == "__trigger__:Complete:notify"
            ))
            .count(),
        1,
        "a hit must not expand an already expanded AST again"
    );
}

#[test]
fn warn_only_hits_still_report_every_violation_and_enforce_rechecks_same_source() {
    // Tracing's callsite interest cache is process-wide. Other parser tests
    // can register this warning while having no thread-local subscriber, so
    // observe the real warnings in an isolated copy of this exact test.
    const CHILD_ENV: &str = "TEMPER_SPEC_WARNING_COUNT_TEST_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "automaton::parse_cache::tests::warn_only_hits_still_report_every_violation_and_enforce_rechecks_same_source",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "isolated warning test failed:\n{}{}",
            stdout,
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    let cache = Arc::new(local_cache(2, 1024));
    let reports = Arc::new(Mutex::new(Vec::<LivenessViolation>::new()));
    let observed = Arc::clone(&reports);
    let reporter_cache = Arc::clone(&cache);
    let reporter = move |violation: &LivenessViolation| {
        // A reporter may itself parse. Never invoke it while holding the cache lock.
        assert!(reporter_cache.try_lock().is_ok());
        observed.lock().unwrap().push(violation.clone());
    };
    let warnings = Arc::new(AtomicUsize::new(0));
    let _subscriber =
        tracing::subscriber::set_default(LivenessWarningCounter(Arc::clone(&warnings)));
    for mode in [
        LivenessEnforcement::WarnOnly,
        LivenessEnforcement::WarnOnly,
        LivenessEnforcement::Enforce,
    ] {
        let result = parse_with_cache(TRAP, mode, || Some(&reporter), &cache);
        match mode {
            LivenessEnforcement::WarnOnly => assert!(result.is_ok()),
            LivenessEnforcement::Enforce => {
                let fresh = prepare_automaton(TRAP).unwrap();
                let expected = check_liveness_coverage(&fresh, mode, || None).unwrap_err();
                assert_eq!(result.unwrap_err().to_string(), expected.to_string());
            }
        }
    }
    let reports = reports.lock().unwrap();
    assert_eq!(
        reports.len(),
        6,
        "two violations on every call, including hits"
    );
    for pair in reports.as_chunks::<2>().0 {
        assert_eq!(pair[0].entity, "CacheLivenessTrap");
        assert_eq!(pair[0].state, "Ready");
        assert_eq!(pair[1].state, "Running");
    }
    assert_eq!(cache.lock().unwrap().entries.len(), 1);
    assert_eq!(
        warnings.load(Ordering::SeqCst),
        4,
        "two warnings on each WarnOnly call"
    );
}

#[test]
fn reporter_is_observed_only_after_liveness_violations_are_known() {
    let cache = local_cache(2, 1024);
    let lookups = AtomicUsize::new(0);
    for source in [PREPARED, PREPARED, TRAP, TRAP] {
        parse_with_cache(
            source,
            LivenessEnforcement::WarnOnly,
            || {
                lookups.fetch_add(1, Ordering::SeqCst);
                None
            },
            &cache,
        )
        .unwrap();
    }
    assert_eq!(lookups.load(Ordering::SeqCst), 2);
}

#[test]
fn rejected_enforcement_does_not_insert_or_evict_successful_sources() {
    let cache = local_cache(1, 1024);
    parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    let snapshot = cache.lock().unwrap().get(PREPARED).unwrap();
    for _ in 0..2 {
        assert!(parse(TRAP, LivenessEnforcement::Enforce, &cache).is_err());
    }
    let cache = cache.lock().unwrap();
    assert!(cache.get(TRAP).is_none());
    assert!(Arc::ptr_eq(&snapshot, &cache.get(PREPARED).unwrap()));
    assert_eq!(cache.source_bytes, PREPARED.len());
}

#[test]
fn exact_source_bytes_distinguish_formatting_and_changed_specs() {
    let cache = local_cache(3, 4096);
    let changed = PREPARED.replace("name = \"Cached\"", "name = \"Changed\"");
    let formatted = format!("{PREPARED}\n# same spec with different source bytes\n");
    let first = parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    let formatting = parse(&formatted, LivenessEnforcement::Enforce, &cache).unwrap();
    let changed_ast = parse(&changed, LivenessEnforcement::Enforce, &cache).unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(formatting).unwrap()
    );
    assert_eq!(changed_ast.automaton.name, "Changed");
    let cache = cache.lock().unwrap();
    assert_eq!(cache.entries.len(), 3);
    assert!(!Arc::ptr_eq(
        &cache.get(PREPARED).unwrap(),
        &cache.get(&formatted).unwrap()
    ));
}

#[test]
fn fifo_eviction_obeys_entry_budget_without_refreshing_hits() {
    let cache = local_cache(2, 4096);
    let second = PREPARED.replace("Cached", "Second");
    let third = PREPARED.replace("Cached", "Third");
    let returned = parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    parse(&second, LivenessEnforcement::Enforce, &cache).unwrap();
    parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    parse(&third, LivenessEnforcement::Enforce, &cache).unwrap();
    let cache = cache.lock().unwrap();
    assert!(cache.get(PREPARED).is_none());
    assert!(cache.get(&second).is_some());
    assert!(cache.get(&third).is_some());
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.insertion_order.len(), 2);
    assert_eq!(cache.source_bytes, second.len() + third.len());
    assert_eq!(
        returned.automaton.name, "Cached",
        "eviction cannot change a returned AST"
    );
}

#[test]
fn source_byte_budget_evicts_before_entry_budget() {
    let second = PREPARED.replace("Cached", "Second");
    let cache = local_cache(8, PREPARED.len() + second.len() - 1);
    parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    parse(&second, LivenessEnforcement::Enforce, &cache).unwrap();
    let cache = cache.lock().unwrap();
    assert!(cache.get(PREPARED).is_none());
    assert!(cache.get(&second).is_some());
    assert_eq!(cache.source_bytes, second.len());
    assert!(cache.source_bytes <= cache.source_byte_budget);
}

#[test]
fn admission_boundary_is_inclusive_but_oversized_and_disabled_caches_still_parse() {
    let at_boundary = format!(
        "{PREPARED}\n#{}",
        "x".repeat(MAX_CACHED_SOURCE_BYTES - PREPARED.len() - 2)
    );
    assert_eq!(at_boundary.len(), MAX_CACHED_SOURCE_BYTES);
    let cache = local_cache(2, TOTAL_SOURCE_BYTE_BUDGET);
    parse(&at_boundary, LivenessEnforcement::Enforce, &cache).unwrap();
    assert_eq!(cache.lock().unwrap().source_bytes, MAX_CACHED_SOURCE_BYTES);

    let oversized = format!("{at_boundary}x");
    for (source, cache) in [
        (oversized.as_str(), local_cache(2, TOTAL_SOURCE_BYTE_BUDGET)),
        (PREPARED, local_cache(0, 1024)),
        (PREPARED, local_cache(2, PREPARED.len() - 1)),
    ] {
        for _ in 0..2 {
            let parsed = parse(source, LivenessEnforcement::Enforce, &cache).unwrap();
            assert_eq!(parsed.automaton.name, "Cached");
        }
        let cache = cache.lock().unwrap();
        assert!(cache.entries.is_empty());
        assert!(cache.insertion_order.is_empty());
        assert_eq!(cache.source_bytes, 0);
    }
}

#[test]
fn source_errors_are_unchanged_and_do_not_enter_or_evict_cache() {
    let cache = local_cache(1, 1024);
    parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    let snapshot = cache.lock().unwrap().get(PREPARED).unwrap();
    let invalid = PREPARED.replace("initial = \"Ready\"", "initial = \"Missing\"");
    for source in ["[automaton\n", invalid.as_str()] {
        let expected = prepare_automaton(source).unwrap_err().to_string();
        for _ in 0..2 {
            let actual = parse(source, LivenessEnforcement::WarnOnly, &cache).unwrap_err();
            assert_eq!(actual.to_string(), expected);
        }
    }
    let cache = cache.lock().unwrap();
    assert_eq!(cache.entries.len(), 1);
    assert!(Arc::ptr_eq(&snapshot, &cache.get(PREPARED).unwrap()));
}

#[test]
fn poisoned_optional_cache_does_not_change_parsing_or_enforcement() {
    let cache = local_cache(1, 1024);
    let poisoned = catch_unwind(AssertUnwindSafe(|| {
        let _guard = cache.lock().unwrap();
        panic!("deliberately poison this test-local cache");
    }));
    assert!(poisoned.is_err());
    assert!(cache.is_poisoned());
    let parsed = parse(PREPARED, LivenessEnforcement::Enforce, &cache).unwrap();
    assert_eq!(parsed.automaton.name, "Cached");
    assert!(parse(TRAP, LivenessEnforcement::WarnOnly, &cache).is_ok());
    assert!(parse(TRAP, LivenessEnforcement::Enforce, &cache).is_err());
    assert!(parse("not TOML", LivenessEnforcement::WarnOnly, &cache).is_err());
}
