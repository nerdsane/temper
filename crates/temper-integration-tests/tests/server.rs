//! Server integration suites, compiled and linked in a shared test harness.
//!
//! Keep source files in their owning crate so source-relative fixtures and
//! nested modules continue to resolve from exactly the same locations.

#[path = "../../temper-server/tests/common/mod.rs"]
mod shared_common;

#[path = "server/actor_runtime_postgres.rs"]
mod actor_runtime_postgres;
#[path = "server/pg_actor_integration.rs"]
mod pg_actor_integration;
#[path = "server/pg_agent_chain.rs"]
mod pg_agent_chain;

// Preserve the original per-suite helper test names and executions without
// recompiling the helper implementations in each suite or test executable.
macro_rules! common {
    () => {
        mod common {
            #[allow(unused_imports)]
            pub use crate::shared_common::*;

            pub mod workload_gen {
                #[allow(unused_imports)]
                pub use crate::shared_common::workload_gen::*;

                mod tests {
                    #[test]
                    fn generator_produces_ops() {
                        crate::shared_common::workload_gen::tests::generator_produces_ops();
                    }

                    #[test]
                    fn generator_is_deterministic() {
                        crate::shared_common::workload_gen::tests::generator_is_deterministic();
                    }
                }
            }
        }
    };
}

#[path = "../../temper-server/tests/adapter_dispatch.rs"]
mod adapter_dispatch;
#[path = "../../temper-server/tests/dispatch_retry_idempotency.rs"]
mod dispatch_retry_idempotency;
#[path = "../../temper-server/tests/dst_concurrency_retry.rs"]
mod dst_concurrency_retry;
#[path = "../../temper-server/tests/dst_entity_key_index.rs"]
mod dst_entity_key_index;
#[path = "../../temper-server/tests/dst_entity_vector_index.rs"]
mod dst_entity_vector_index;
#[path = "../../temper-server/tests/dst_genesis_install_rollback.rs"]
mod dst_genesis_install_rollback;
#[path = "../../temper-server/tests/dst_hotswap.rs"]
mod dst_hotswap;
#[path = "../../temper-server/tests/dst_lifecycle.rs"]
mod dst_lifecycle;
#[path = "../../temper-server/tests/dst_multi_tenant.rs"]
mod dst_multi_tenant;
#[path = "../../temper-server/tests/dst_persistence.rs"]
mod dst_persistence;
#[path = "../../temper-server/tests/dst_platform_boot.rs"]
mod dst_platform_boot;
#[path = "../../temper-server/tests/dst_platform_cedar.rs"]
mod dst_platform_cedar;
#[path = "../../temper-server/tests/dst_platform_index.rs"]
mod dst_platform_index;
#[path = "../../temper-server/tests/dst_platform_random.rs"]
mod dst_platform_random;
#[path = "../../temper-server/tests/dst_platform_rollback.rs"]
mod dst_platform_rollback;
#[path = "../../temper-server/tests/e2e_gepa_loop.rs"]
mod e2e_gepa_loop;
#[path = "../../temper-server/tests/ensure_entity_loaded.rs"]
mod ensure_entity_loaded;
#[path = "../../temper-server/tests/field_invariants.rs"]
mod field_invariants;
#[path = "../../temper-server/tests/file_value_fast_path.rs"]
mod file_value_fast_path;
#[path = "../../temper-server/tests/gepa_manual_verification.rs"]
mod gepa_manual_verification;
#[path = "../../temper-server/tests/gmail_oauth_dst.rs"]
mod gmail_oauth_dst;
#[path = "../../temper-server/tests/multi_tenant.rs"]
mod multi_tenant;
#[path = "../../temper-server/tests/nearest_odata.rs"]
mod nearest_odata;
#[path = "../../temper-server/tests/odata_read.rs"]
mod odata_read;
#[path = "../../temper-server/tests/passivation_respawn.rs"]
mod passivation_respawn;
#[path = "../../temper-server/tests/policy_authorization.rs"]
mod policy_authorization;
#[path = "../../temper-server/tests/published_artifacts.rs"]
mod published_artifacts;
#[path = "../../temper-server/tests/query_projection_backfill.rs"]
mod query_projection_backfill;
#[path = "../../temper-server/tests/reaction_cascade.rs"]
mod reaction_cascade;
#[path = "../../temper-server/tests/reaction_e2e_prod.rs"]
mod reaction_e2e_prod;
#[path = "../../temper-server/tests/resource_authorization.rs"]
mod resource_authorization;
#[path = "../../temper-server/tests/spec_validate_endpoint.rs"]
mod spec_validate_endpoint;
#[path = "../../temper-server/tests/storage_stack.rs"]
mod storage_stack;
#[path = "../../temper-server/tests/strict_action_contract.rs"]
mod strict_action_contract;
#[path = "../../temper-server/tests/strict_creation_boundaries.rs"]
mod strict_creation_boundaries;
#[path = "../../temper-server/tests/strict_generic_writes.rs"]
mod strict_generic_writes;
#[path = "../../temper-server/tests/strict_native_callbacks.rs"]
mod strict_native_callbacks;
#[path = "../../temper-server/tests/strict_postgres_actions.rs"]
mod strict_postgres_actions;
#[path = "../../temper-server/tests/trajectory_capture.rs"]
mod trajectory_capture;
#[path = "../../temper-server/tests/trigger_e2e_prod.rs"]
mod trigger_e2e_prod;
#[path = "../../temper-server/tests/wasm_dispatch.rs"]
mod wasm_dispatch;
#[path = "../../temper-server/tests/wasm_dispatch_observability_contract.rs"]
mod wasm_dispatch_observability_contract;

#[test]
fn integration_suite_inventory_matches_source_files() {
    use std::collections::BTreeSet;
    use std::path::Path;

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for owner in ["server", "platform"] {
        let harness = std::fs::read_to_string(manifest_dir.join(format!("tests/{owner}.rs")))
            .expect("integration harness source should exist");
        let prefix = format!("#[path = \"../../temper-{owner}/tests/");
        let declared: BTreeSet<_> = harness
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix(prefix.as_str())
                    .and_then(|path| path.strip_suffix("\"]"))
            })
            .filter(|path| !path.contains('/'))
            .map(str::to_owned)
            .collect();
        assert!(!declared.is_empty(), "{owner} harness must declare suites");

        let source_files: BTreeSet<_> =
            std::fs::read_dir(manifest_dir.join(format!("../temper-{owner}/tests")))
                .expect("test source directory should exist")
                .map(|entry| entry.expect("test source entry should be readable").path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "rs"))
                .map(|path| {
                    path.file_name()
                        .expect("suite should have a file name")
                        .to_str()
                        .expect("suite file name should be UTF-8")
                        .to_owned()
                })
                .collect();

        assert_eq!(
            declared, source_files,
            "register every {owner} integration source file in its shared harness"
        );
    }
    actor_runtime_postgres::assert_inventory_matches_source_files(manifest_dir);
}
