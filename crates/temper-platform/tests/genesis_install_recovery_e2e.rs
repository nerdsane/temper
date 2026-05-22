use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use temper_platform::state::PlatformState;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::tenant::TenantId;
use temper_server::event_store::ServerEventStore;
use temper_server::registry_bootstrap::restore_registry_from_turso;
use temper_store_turso::TursoEventStore;

const TARGET_TENANT: &str = "genesis-target";
const APP_NAME: &str = "project-management";
const APP_REF: &str = "acme/project-management@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const VERSION_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const APP_ID: &str = "app-acme-project-management";
const INSTALLATION_ID: &str = "ai-app-acme-project-management-genesis-target-aaaaaaaaaaaaaaaa";

async fn new_state(db_url: &str) -> PlatformState {
    let mut state = PlatformState::new(None);
    let turso = TursoEventStore::new(db_url, None).await.unwrap();
    state.server.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));
    state
}

async fn persist_installed_app_fixture(state: &PlatformState) {
    let turso = state
        .server
        .event_store
        .as_ref()
        .unwrap()
        .platform_turso_store()
        .unwrap();
    turso
        .record_installed_app(TARGET_TENANT, APP_NAME)
        .await
        .unwrap();
    turso.commit_specs(TARGET_TENANT).await.unwrap();
}

async fn persist_genesis_recovery_fixture(state: &PlatformState) {
    let turso = state
        .server
        .event_store
        .as_ref()
        .unwrap()
        .platform_turso_store()
        .unwrap();

    let cache_root = std::env::temp_dir()
        .join("temper-genesis-app-cache")
        .join("acme-project-management-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let app_dir = cache_root.join(APP_NAME);
    std::fs::create_dir_all(app_dir.join("specs")).unwrap();
    std::fs::write(
        app_dir.join("app.toml"),
        "name = \"project-management\"\ndependencies = []\n",
    )
    .unwrap();
    std::fs::write(
        app_dir.join("specs").join("issue.ioa.toml"),
        include_str!("../../../os-apps/project-management/issue.ioa.toml"),
    )
    .unwrap();

    let app_pid = format!("default:App:{APP_ID}");
    let app_install_pid = format!("default:AppInstallation:{INSTALLATION_ID}");

    turso
        .append(
            &app_pid,
            0,
            &[envelope(
                1,
                "Defined",
                serde_json::json!({
                    "OwnerId": "acme",
                    "Name": APP_NAME,
                    "RepositoryId": "repo-1",
                    "LatestVersionHash": VERSION_HASH,
                }),
                &app_pid,
            )],
        )
        .await
        .unwrap();
    turso
        .append(
            &app_install_pid,
            0,
            &[envelope(
                1,
                "Issued",
                serde_json::json!({
                    "AppId": APP_ID,
                    "AppRef": APP_REF,
                    "TargetTenant": TARGET_TENANT,
                    "VersionHash": VERSION_HASH,
                    "Installer": "test-harness",
                }),
                &app_install_pid,
            )],
        )
        .await
        .unwrap();
    turso
        .append(
            &app_install_pid,
            1,
            &[envelope(
                2,
                "MarkedInstalled",
                serde_json::json!({
                    "ClosureId": format!("genesis:{APP_REF}:{VERSION_HASH}"),
                    "Message": "installed",
                    "InstalledAt": "2026-05-22T12:00:00Z",
                }),
                &app_install_pid,
            )],
        )
        .await
        .unwrap();
}

fn envelope(
    sequence_nr: u64,
    event_type: &str,
    payload: serde_json::Value,
    actor_id: &str,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: Utc::now(),
            actor_id: actor_id.to_string(),
        },
    }
}

#[tokio::test]
async fn e2e_genesis_install_persists_and_recovers() {
    let db_path = format!("/tmp/temper-genesis-install-{}.db", uuid::Uuid::new_v4());
    let db_url = format!("file:{db_path}");

    let state = new_state(&db_url).await;
    persist_installed_app_fixture(&state).await;
    persist_genesis_recovery_fixture(&state).await;

    let restarted = new_state(&db_url).await;

    {
        use temper_server::registry::SpecRegistry;
        let mut temp_registry = SpecRegistry::new();
        let restored = restore_registry_from_turso(
            &mut temp_registry,
            restarted
                .server
                .event_store
                .as_ref()
                .unwrap()
                .platform_turso_store()
                .unwrap(),
        )
        .await
        .unwrap();
        let _ = restored;
        *restarted.registry.write().unwrap() = temp_registry;
    }

    temper_platform::genesis_install::restore_genesis_app_cache_roots(&restarted).await;
    temper_platform::install_os_app(&restarted, TARGET_TENANT, APP_NAME)
        .await
        .unwrap();

    let target_tenant = TenantId::new(TARGET_TENANT);
    {
        let registry = restarted.registry.read().unwrap();
        assert!(registry.get_table(&target_tenant, "Issue").is_some());
        assert!(registry.get_table(&target_tenant, "Project").is_some());
    }

    let cache_root: PathBuf = std::env::temp_dir()
        .join("temper-genesis-app-cache")
        .join("acme-project-management-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(cache_root.join(APP_NAME).join("app.toml").is_file());

    let rows = restarted
        .server
        .event_store
        .as_ref()
        .unwrap()
        .read_events(&format!("default:AppInstallation:{INSTALLATION_ID}"), 0)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].event_type, "MarkedInstalled");

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
    let _ = std::fs::remove_dir_all(cache_root);
}
