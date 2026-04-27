/// Test utilities: Postgres testcontainer + schema setup.
use deadpool_postgres::{Config as PgConfig, Pool};
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;

/// Start a fresh Postgres testcontainer, apply the actor runtime schema, and return a pool.
/// Each call creates a new isolated container — drop the ContainerAsync to stop it.
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
