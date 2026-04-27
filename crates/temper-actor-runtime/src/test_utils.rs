/// Test utilities: Postgres testcontainer + schema setup.
use std::sync::OnceLock;

use deadpool_postgres::{Config as PgConfig, Pool};
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

/// Start a fresh isolated Postgres container per test. Drop ContainerAsync to stop it.
pub async fn setup_test_pg() -> (Pool, ContainerAsync<Postgres>) {
    use testcontainers::runners::AsyncRunner;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");

    let host = container.get_host().await.expect("get host");
    let port = container.get_host_port_ipv4(5432).await.expect("get port");

    let mut cfg = PgConfig::new();
    cfg.host = Some(host.to_string());
    cfg.port = Some(port);
    cfg.user = Some("postgres".to_string());
    cfg.password = Some("postgres".to_string());
    cfg.dbname = Some("postgres".to_string());

    let pool = cfg
        .create_pool(None, tokio_postgres::NoTls)
        .expect("create pool");

    let client = pool.get().await.expect("get client");
    crate::schema::create_tables(&client)
        .await
        .expect("apply schema");

    (pool, container)
}

/// Start a shared Postgres container (one per test binary, container leaked intentionally).
/// Use with `OnceCell` for test suites that share a pool across multiple tests.
pub async fn setup_shared_pg() -> Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    if let Some(p) = POOL.get() {
        return p.clone();
    }
    use testcontainers::runners::AsyncRunner;

    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start Postgres container");

    let host = container.get_host().await.expect("get host");
    let port = container.get_host_port_ipv4(5432).await.expect("get port");

    // Leak so the container lives for the entire test binary lifetime.
    Box::leak(Box::new(container));

    let mut cfg = PgConfig::new();
    cfg.host = Some(host.to_string());
    cfg.port = Some(port);
    cfg.user = Some("postgres".to_string());
    cfg.password = Some("postgres".to_string());
    cfg.dbname = Some("postgres".to_string());

    let pool = cfg
        .create_pool(None, tokio_postgres::NoTls)
        .expect("create pool");

    let client = pool.get().await.expect("get client");
    crate::schema::create_tables(&client)
        .await
        .expect("apply schema");

    POOL.get_or_init(|| pool.clone()).clone()
}
