//! Run the original actor-runtime PostgreSQL proofs in the shared server harness.
//!
//! Docker startup can overlap server scenarios, without moving proof bodies or
//! changing their fixtures. The actor-runtime feature suppresses only duplicate
//! unit- and integration-test registrations; standalone defaults still run them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

macro_rules! postgres_proofs {
    ($(#[$runtime:meta] $name:ident => $proof:path),+ $(,)?) => {
        $(
            #[$runtime]
            async fn $name() {
                $proof().await;
            }
        )+

        /// Original proof paths and exact Tokio runtime attributes for inventory checks.
        pub(super) const REGISTERED_PROOFS: &[(&str, &str, &str)] = &[
            $((stringify!($name), stringify!($proof), stringify!($runtime))),+
        ];
    };
}

/// Share exact-runtime proof registration with the later actor and agent groups.
pub(super) use postgres_proofs;

postgres_proofs! {
    #[tokio::test]
    rejected_input_is_consumed_and_the_next_valid_message_runs => temper_actor_runtime::pg::strict_tests::rejected_input_is_consumed_and_the_next_valid_message_runs,
    #[tokio::test]
    rejection_discards_handler_mutations_and_tells_but_transient_failure_retries => temper_actor_runtime::pg::strict_tests::rejection_discards_handler_mutations_and_tells_but_transient_failure_retries,
    #[tokio::test]
    routed_trigger_projects_only_declared_inputs_then_enforces_constraints => temper_actor_runtime::pg::strict_tests::routed_trigger_projects_only_declared_inputs_then_enforces_constraints,
    #[tokio::test]
    fresh_identity_is_persisted_before_any_action => temper_actor_runtime::pg::strict_tests::fresh_identity_is_persisted_before_any_action,
    #[tokio::test]
    activation_preserves_recovered_bytes_and_initializes_only_absent_actors => temper_actor_runtime::pg::strict_tests::activation_preserves_recovered_bytes_and_initializes_only_absent_actors,
    #[tokio::test]
    auxiliary_state_updates_invalidate_queued_authorization => temper_actor_runtime::pg::strict_tests::auxiliary_state_updates_invalidate_queued_authorization,
    #[tokio::test]
    public_spawn_with_fields_validates_strict_creation_before_writing => temper_actor_runtime::pg::strict_tests::creation_tests::public_spawn_with_fields_validates_strict_creation_before_writing,
    #[tokio::test]
    context_spawn_persists_strict_child_defaults_before_activation => temper_actor_runtime::pg::strict_tests::creation_tests::context_spawn_persists_strict_child_defaults_before_activation,
    #[tokio::test]
    public_creation_normalizes_both_aliases_and_refuses_conflicting_identity => temper_actor_runtime::pg::strict_tests::creation_tests::public_creation_normalizes_both_aliases_and_refuses_conflicting_identity,
    #[tokio::test]
    overlapping_container_leases_keep_databases_and_locks_isolated => temper_actor_runtime::test_utils::tests::overlapping_container_leases_keep_databases_and_locks_isolated,
}

/// Guard exact coverage and mutually exclusive original/wrapper registration.
pub(super) fn assert_inventory_matches_source_files(manifest_dir: &Path) {
    const REGISTRATION_PREFIX: &str = "#[cfg_attr(not(feature=\"test-shared-pg-proofs\"),";
    let mut source_proofs = BTreeMap::new();
    for (owner, crate_name, source_file, module) in [
        (
            "actor-runtime",
            "temper_actor_runtime",
            "src/pg_strict_tests.rs",
            "pg::strict_tests",
        ),
        (
            "actor-runtime",
            "temper_actor_runtime",
            "src/pg_creation_tests.rs",
            "pg::strict_tests::creation_tests",
        ),
        (
            "actor-runtime",
            "temper_actor_runtime",
            "src/test_utils_tests.rs",
            "test_utils::tests",
        ),
        (
            "actor-runtime",
            "temper_actor_runtime",
            "tests/integration.rs",
            "integration_proofs",
        ),
        (
            "actor-runtime",
            "temper_actor_runtime",
            "tests/integration/creation_race.rs",
            "integration_proofs::creation_race",
        ),
        (
            "agents",
            "temper_agents",
            "tests/agent_chain.rs",
            "agent_chain_proofs",
        ),
    ] {
        let source = std::fs::read_to_string(
            manifest_dir
                .join(format!("../temper-{owner}"))
                .join(source_file),
        )
        .expect("PostgreSQL proof source should exist");
        let lines: Vec<_> = source.lines().map(str::trim).collect();
        let mut registration_lines = BTreeSet::new();
        for (index, line) in lines.iter().enumerate() {
            if let Some(name) = line
                .strip_prefix("pub async fn ")
                .and_then(|signature| signature.strip_suffix("() {"))
            {
                let start = lines[..index]
                    .iter()
                    .rposition(|previous| previous.starts_with("#["))
                    .expect("exported proofs must retain their registration attribute");
                let registration = compact(&lines[start..index].concat());
                let runtime = registration
                    .strip_prefix(REGISTRATION_PREFIX)
                    .and_then(|attribute| attribute.strip_suffix(")]"))
                    .filter(|attribute| attribute.starts_with("tokio::test"))
                    .unwrap_or_else(|| {
                        panic!("{source_file}::{name} must retain its standalone registration")
                    });
                registration_lines.extend(start..index);
                assert!(
                    source_proofs
                        .insert(
                            format!("{crate_name}::{module}::{name}"),
                            runtime.to_owned()
                        )
                        .is_none(),
                    "duplicate {owner} PostgreSQL proof {module}::{name}"
                );
            }
        }
        for (index, line) in lines.iter().enumerate() {
            if line.contains("tokio::test") {
                assert!(
                    registration_lines.contains(&index),
                    "{source_file} proof registration must be exclusive and exported for the shared harness"
                );
            }
        }
    }

    let mut registered_proofs = BTreeMap::new();
    let mut wrapper_names = BTreeSet::new();
    for (name, path, runtime) in REGISTERED_PROOFS
        .iter()
        .chain(crate::pg_actor_integration::REGISTERED_PROOFS)
        .chain(crate::pg_agent_chain::REGISTERED_PROOFS)
    {
        let path = compact(path);
        assert_eq!(
            path.rsplit("::").next(),
            Some(*name),
            "wrapper must keep its proof name"
        );
        assert!(
            wrapper_names.insert(*name),
            "duplicate PostgreSQL wrapper {name}"
        );
        assert!(
            registered_proofs.insert(path, compact(runtime)).is_none(),
            "each PostgreSQL proof must run exactly once"
        );
    }
    assert!(!source_proofs.is_empty(), "PostgreSQL proofs must exist");
    assert_eq!(
        registered_proofs, source_proofs,
        "register every PostgreSQL proof exactly once with its original Tokio runtime"
    );
}

fn compact(tokens: &str) -> String {
    tokens
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
