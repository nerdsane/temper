//! Legacy reads return only the requested byte range, with the actual size.
use temper_store_postgres::{PostgresEventStore, migration::run_migrations};

#[tokio::test]
async fn legacy_blob_chunks_preserve_bytes_and_report_actual_length() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    run_migrations(&pool).await.unwrap();
    let store = PostgresEventStore::new(pool);
    let key = format!("chunk-test-{}", uuid::Uuid::new_v4());
    let bytes: Vec<u8> = (0..(3 * 64 * 1024 + 7)).map(|n| (n % 256) as u8).collect();
    store.put_blob(&key, &bytes).await.unwrap();
    assert_eq!(
        store.get_blob_chunk(&key, 0, 0).await.unwrap(),
        Some((bytes.len() as u64, vec![]))
    );
    for offset in [0, 65536, 131072, 196608, bytes.len(), bytes.len() + 1] {
        let (length, chunk) = store
            .get_blob_chunk(&key, offset as u64, 65536)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(length, bytes.len() as u64);
        let start = offset.min(bytes.len());
        assert_eq!(chunk, bytes[start..(start + 65536).min(bytes.len())]);
    }
    assert_eq!(
        store
            .get_blob_chunk(&format!("{key}-absent"), 0, 65536)
            .await
            .unwrap(),
        None
    );
}
