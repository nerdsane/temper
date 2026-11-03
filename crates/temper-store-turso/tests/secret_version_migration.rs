//! Legacy secret rows receive stable versions, including rows inserted after rollback.

use temper_store_turso::TursoEventStore;

#[tokio::test]
async fn legacy_secret_versions_are_backfilled_without_replacing_existing_versions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.db");
    let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE tenant_secrets (
                tenant TEXT NOT NULL, key_name TEXT NOT NULL,
                ciphertext BLOB NOT NULL, nonce BLOB NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (tenant, key_name)
             );
             INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce)
                VALUES ('tenant-a', 'signin', X'01', X'02'),
                       ('tenant-b', 'signin', X'03', X'04');",
        )
        .await
        .unwrap();
    drop(connection);
    drop(database);

    let url = format!("file:{}", path.display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let a = store.list_secret_versions("tenant-a").await.unwrap();
    let b = store.list_secret_versions("tenant-b").await.unwrap();
    for versions in [&a, &b] {
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].0, "signin");
        let version = versions[0].1.as_ref().expect("legacy row backfilled");
        assert_eq!(version.len(), 32);
        assert!(version.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    assert_ne!(a[0].1, b[0].1, "each legacy row gets its own version");
    assert_eq!(
        store.load_secrets_for_tenant("tenant-a").await.unwrap(),
        vec![("signin".into(), vec![1], vec![2])]
    );
    drop(store);

    // An older release omits version_id when inserting after a rollback.
    let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection
        .execute(
            "INSERT INTO tenant_secrets (tenant, key_name, ciphertext, nonce)
             VALUES ('tenant-c', 'signin', X'05', X'06')",
            (),
        )
        .await
        .unwrap();
    drop(connection);
    drop(database);

    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(store.list_secret_versions("tenant-a").await.unwrap(), a);
    assert_eq!(store.list_secret_versions("tenant-b").await.unwrap(), b);
    let c = store.list_secret_versions("tenant-c").await.unwrap();
    assert_eq!(c[0].1.as_ref().expect("rollback row backfilled").len(), 32);
    assert_ne!(c[0].1, a[0].1);
    assert_ne!(c[0].1, b[0].1);
    drop(store);

    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(store.list_secret_versions("tenant-a").await.unwrap(), a);
    assert_eq!(store.list_secret_versions("tenant-b").await.unwrap(), b);
    assert_eq!(store.list_secret_versions("tenant-c").await.unwrap(), c);
}
