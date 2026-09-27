//! Container leases preserve separate databases, sessions, and cleanup ownership.

use super::*;
use crate::schema;

/// Prove overlapping container leases retain database, lock, and cleanup isolation.
#[cfg_attr(not(feature = "test-shared-pg-proofs"), tokio::test)]
pub async fn overlapping_container_leases_keep_databases_and_locks_isolated() {
    if let Ok(url) = std::env::var("TEMPER_ACTOR_TEST_DATABASE_URL") {
        let parsed: tokio_postgres::Config = url.parse().expect("valid local test URL");
        let (pool, container) = setup_test_pg().await;
        assert!(
            container.is_none(),
            "external fixture must not start Docker"
        );
        let client = pool.get().await.unwrap();
        let row = client
            .query_one(
                "SELECT current_database(), \
                 to_regclass('odp_temper.actor_instances') IS NOT NULL, \
                 to_regclass('odp_temper.actor_messages') IS NOT NULL, \
                 to_regclass('odp_temper.actor_types') IS NOT NULL",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), parsed.get_dbname().unwrap());
        for (index, table) in [
            (1, "odp_temper.actor_instances"),
            (2, "odp_temper.actor_messages"),
            (3, "odp_temper.actor_types"),
        ] {
            assert!(row.get::<_, bool>(index), "missing fixture table {table}");
        }
        client
            .query(
                schema::LOAD_ACTOR,
                &[&"lease-route-proof", &"LeaseRouteProof"],
            )
            .await
            .unwrap();
        return;
    }

    // Local sharing state makes the last-lease check independent of other tests.
    let shared = Mutex::new(Weak::new());
    let ((first_pool, first_container), (second_pool, second_container)) = tokio::join!(
        setup_isolated_test_pg(&shared),
        setup_isolated_test_pg(&shared),
    );
    assert!(Arc::ptr_eq(&first_container, &second_container));
    assert_eq!(first_container.id(), second_container.id());
    let weak = Arc::downgrade(&first_container);

    let mut first = first_pool.get().await.unwrap();
    let mut second = second_pool.get().await.unwrap();
    let first_database: String = first
        .query_one("SELECT current_database()", &[])
        .await
        .unwrap()
        .get(0);
    let second_database: String = second
        .query_one("SELECT current_database()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(first_database, second_database);
    assert!(first_database.starts_with("temper_test_"));
    assert!(second_database.starts_with("temper_test_"));

    let namespace = "same-namespace";
    let actor_type = "SameActor";
    for (client, state) in [
        (&first, b"first database".to_vec()),
        (&second, b"second database".to_vec()),
    ] {
        assert_eq!(
            client
                .execute(schema::CREATE_ACTOR, &[&namespace, &actor_type, &state])
                .await
                .unwrap(),
            1,
        );
        let persisted: Vec<u8> = client
            .query_one(schema::LOAD_ACTOR, &[&namespace, &actor_type])
            .await
            .unwrap()
            .get("state");
        assert_eq!(persisted, state);
    }

    let first_transaction = first.transaction().await.unwrap();
    let second_transaction = second.transaction().await.unwrap();
    for transaction in [&first_transaction, &second_transaction] {
        let locked: bool = transaction
            .query_one(schema::TRY_ADVISORY_XACT_LOCK, &[&namespace, &actor_type])
            .await
            .unwrap()
            .get(0);
        assert!(locked, "another database must not hold this actor's lock");
    }
    let mut competing_client = first_pool.get().await.unwrap();
    let competing_transaction = competing_client.transaction().await.unwrap();
    let locked: bool = competing_transaction
        .query_one(schema::TRY_ADVISORY_XACT_LOCK, &[&namespace, &actor_type])
        .await
        .unwrap()
        .get(0);
    assert!(
        !locked,
        "another session in this database must see the lock"
    );
    competing_transaction.rollback().await.unwrap();
    first_transaction.rollback().await.unwrap();
    second_transaction.rollback().await.unwrap();

    drop(competing_client);
    drop(first);
    drop(first_pool);
    drop(first_container);
    assert!(
        weak.upgrade().is_some(),
        "the second lease must keep PostgreSQL alive"
    );
    assert_eq!(
        second
            .query_one("SELECT 1", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        1,
    );

    drop(second);
    drop(second_pool);
    drop(second_container);
    assert!(
        weak.upgrade().is_none(),
        "sharing state must not own the container"
    );
    assert!(shared.lock().await.upgrade().is_none());
}
