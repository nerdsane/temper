/// Test utilities: Postgres testcontainer + schema setup.
use std::sync::OnceLock;

use deadpool_postgres::{Config as PgConfig, Pool};
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

/// Start a fresh isolated Postgres container per test. Drop ContainerAsync to stop it.
pub async fn setup_test_pg() -> (Pool, Option<ContainerAsync<Postgres>>) {
    if let Ok(url) = std::env::var("TEMPER_ACTOR_TEST_DATABASE_URL") {
        let parsed: tokio_postgres::Config = url.parse().expect("valid local test URL");
        assert!(
            parsed.get_hosts().iter().all(|host| matches!(host,
            tokio_postgres::config::Host::Tcp(name) if name == "127.0.0.1" || name == "localhost"))
        );
        assert!(
            parsed
                .get_dbname()
                .is_some_and(|name| name.starts_with("temper_test_"))
        );
        let mut cfg = PgConfig::new();
        cfg.url = Some(url);
        let pool = cfg
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .expect("create local test pool");
        static SCHEMA: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
        SCHEMA
            .get_or_init(|| async {
                crate::schema::create_tables(&pool.get().await.unwrap())
                    .await
                    .unwrap();
            })
            .await;
        return (pool, None);
    }
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
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .expect("create pool");

    let client = pool.get().await.expect("get client");
    crate::schema::create_tables(&client)
        .await
        .expect("apply schema");

    (pool, Some(container))
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
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .expect("create pool");

    let client = pool.get().await.expect("get client");
    crate::schema::create_tables(&client)
        .await
        .expect("apply schema");

    POOL.get_or_init(|| pool.clone()).clone()
}
