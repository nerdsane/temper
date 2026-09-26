use readers_writers_reference::{MODEL_CSDL, READERS_WRITERS_IOA};
use temper_jit::table::{Effect, Expr, TransitionTable};
use temper_spec::automaton::{lint_automaton, parse_automaton};
use temper_spec::csdl::parse_csdl;
use temper_spec::predicate::parse_arg;
use temper_verify::cascade::{CascadeLevel, VerificationCascade};

#[test]
fn ioa_parses_and_expands_wasm_triggers() {
    let automaton = parse_automaton(READERS_WRITERS_IOA).expect("ReadersWriters IOA should parse");
    assert_eq!(automaton.automaton.name, "ReadersWriters");
    assert_eq!(automaton.automaton.initial, "Idle");

    let findings = lint_automaton(&automaton);
    let errors: Vec<_> = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.severity,
                temper_spec::automaton::LintSeverity::Error
            )
        })
        .collect();
    assert!(errors.is_empty(), "lint errors: {errors:#?}");

    let triggers: Vec<_> = automaton
        .integrations
        .iter()
        .filter(|integration| integration.integration_type == "wasm")
        .map(|integration| integration.trigger.as_str())
        .collect();
    assert!(triggers.contains(&"__trigger__:TryRead:step"));
    assert!(triggers.contains(&"__trigger__:ValidateProposal:validate"));
}

#[test]
fn csdl_exposes_protocol_and_callback_actions() {
    let csdl = parse_csdl(MODEL_CSDL).expect("ReadersWriters CSDL should parse");
    let schema = csdl
        .schemas
        .iter()
        .find(|schema| schema.namespace == "Temper.ReadersWriters")
        .expect("Temper.ReadersWriters schema should exist");

    for action in [
        "TryRead",
        "TryWrite",
        "ReadOrWrite",
        "Stop",
        "ValidateProposal",
        "Enqueued",
        "ReaderStartedFromIdle",
        "ReaderStartedFromReading",
        "WriterStarted",
        "ReaderStoppedMoreRemain",
        "ReaderStoppedLast",
        "WriterStopped",
        "Rejected",
    ] {
        assert!(schema.action(action).is_some(), "missing action {action}");
    }
}

#[test]
fn transition_table_encodes_safety_critical_callbacks() {
    let table = TransitionTable::from_ioa_source(READERS_WRITERS_IOA);

    let writer_started = table
        .rules
        .iter()
        .find(|rule| rule.name == "WriterStarted")
        .expect("WriterStarted rule");
    assert_eq!(writer_started.from_states, vec!["Idle".to_string()]);
    assert_eq!(writer_started.to_state.as_deref(), Some("Writing"));
    assert!(guard_contains(
        &writer_started.guard,
        &expr("wasm_in_flight")
    ));
    assert!(writer_started.effects.contains(&Effect::AddCounter {
        var: "writer_count".into(),
        value: parse_arg("1").unwrap(),
    }));
    assert!(writer_started.effects.contains(&Effect::SubCounter {
        var: "waiting_count".into(),
        value: parse_arg("1").unwrap(),
    }));

    let reader_started = table
        .rules
        .iter()
        .find(|rule| rule.name == "ReaderStartedFromReading")
        .expect("ReaderStartedFromReading rule");
    assert_eq!(reader_started.from_states, vec!["Reading".to_string()]);
    assert_eq!(reader_started.to_state.as_deref(), Some("Reading"));
    assert!(reader_started.effects.contains(&Effect::AddCounter {
        var: "reader_count".into(),
        value: parse_arg("1").unwrap(),
    }));
}

#[test]
fn transition_table_encodes_wasm_in_flight_guard() {
    let table = TransitionTable::from_ioa_source(READERS_WRITERS_IOA);

    for action_name in ["TryRead", "TryWrite", "ReadOrWrite", "Stop"] {
        let rule = table
            .rules
            .iter()
            .find(|rule| rule.name == action_name)
            .unwrap_or_else(|| panic!("{action_name} rule"));
        assert!(
            guard_contains(&rule.guard, &expr("!wasm_in_flight")),
            "{action_name} should be guarded by wasm_in_flight == false"
        );
        assert!(
            rule.effects.contains(&Effect::SetBool {
                var: "wasm_in_flight".to_string(),
                value: parse_arg("true").unwrap(),
            }),
            "{action_name} should set wasm_in_flight before invoking WASM"
        );
    }

    for action_name in [
        "Enqueued",
        "Unchanged",
        "ReaderStartedFromIdle",
        "ReaderStartedFromReading",
        "WriterStarted",
        "ReaderStoppedMoreRemain",
        "ReaderStoppedLast",
        "WriterStopped",
        "Rejected",
        "StepFailed",
    ] {
        let rule = table
            .rules
            .iter()
            .find(|rule| rule.name == action_name)
            .unwrap_or_else(|| panic!("{action_name} rule"));
        assert!(
            guard_contains(&rule.guard, &expr("wasm_in_flight")),
            "{action_name} should only run while a WASM step is in flight"
        );
        assert!(
            rule.effects.contains(&Effect::SetBool {
                var: "wasm_in_flight".to_string(),
                value: parse_arg("false").unwrap(),
            }),
            "{action_name} should clear wasm_in_flight"
        );
    }
}

fn expr(source: &str) -> Expr {
    temper_spec::predicate::parse(source).unwrap()
}

/// Whether `expected` is `actual` or one of its top-level conjuncts.
fn guard_contains(actual: &Expr, expected: &Expr) -> bool {
    actual == expected || matches!(actual, Expr::And(parts) if parts.contains(expected))
}

#[test]
fn verification_cascade_passes_projected_safety() {
    let cascade = VerificationCascade::from_ioa(READERS_WRITERS_IOA)
        .with_sim_seeds(4)
        .with_prop_test_cases(100);

    let result = cascade.run();
    for level in &result.levels {
        assert!(level.passed, "cascade level failed: {}", level.summary);
    }
    assert!(
        result
            .level_result(CascadeLevel::ModelCheck)
            .expect("model check result")
            .passed
    );
    assert!(result.all_passed, "cascade should pass all levels");
}
