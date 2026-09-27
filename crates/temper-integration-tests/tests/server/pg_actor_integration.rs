//! Original actor integration proofs, with their original Tokio runtime settings.
//!
//! This later-sorting module leaves worker slots available for CPU-only scenarios
//! while the earlier PostgreSQL proofs start Docker. Only registration changes;
//! proof bodies, fixtures, and the external database opt-in remain in their owner.

use crate::actor_runtime_postgres::postgres_proofs;

postgres_proofs! {
    #[tokio::test]
    test_tell_and_activate => temper_actor_runtime::integration_proofs::test_tell_and_activate,
    #[tokio::test]
    test_ping_pong_3_rounds => temper_actor_runtime::integration_proofs::test_ping_pong_3_rounds,
    #[tokio::test]
    test_no_pending_messages_skip => temper_actor_runtime::integration_proofs::test_no_pending_messages_skip,
    #[tokio::test]
    test_no_reprocessing => temper_actor_runtime::integration_proofs::test_no_reprocessing,
    #[tokio::test]
    test_fifo_ordering => temper_actor_runtime::integration_proofs::test_fifo_ordering,
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    activation_preserves_fields_from_concurrent_creation => temper_actor_runtime::integration_proofs::creation_race::activation_preserves_fields_from_concurrent_creation,
}
