//! Original agent-chain PG proofs, with unchanged runtime and polling behavior.
//!
//! This later-sorting group leaves worker slots available for CPU-only scenarios
//! while the earlier PostgreSQL proofs start their fixtures.

use crate::actor_runtime_postgres::postgres_proofs;

postgres_proofs! {
    #[tokio::test]
    test_agent_initialize => temper_agents::agent_chain_proofs::test_agent_initialize,
    #[tokio::test]
    test_inference_chain_discovers_unspawned_registered_siblings => temper_agents::agent_chain_proofs::test_inference_chain_discovers_unspawned_registered_siblings,
    #[tokio::test]
    test_context_manager_transitions => temper_agents::agent_chain_proofs::test_context_manager_transitions,
}
