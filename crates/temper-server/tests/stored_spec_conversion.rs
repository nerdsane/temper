//! A stored spec in the old predicate syntax is converted once, at the first
//! boot that reads it, and written back; no later boot converts it again. The
//! text it was stored with is kept beside it, byte for byte, so the
//! conversion can be undone.
//!
//! A server with many tenants restored thousands of stored specs on every
//! boot, and converted each old-syntax one in memory each time without
//! writing it back, so every boot paid for every conversion again. Each parse
//! of a stored spec also logged its liveness gaps and kernel-owned CSDL names
//! at warn, thousands of lines per boot. A restore reads what an install
//! already reported, so it logs neither.
//!
//! The Postgres case needs `TEMPER_TEST_POSTGRES_URL` naming a disposable
//! database; the Turso case runs anywhere.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{PlatformStore, SpecVerificationUpdate};
use temper_server::registry::{SpecRegistry, VerificationStatus};
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres_store, restore_registry_from_turso,
};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};
use temper_store_turso::TursoEventStore;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

/// A spec stored before the predicate grammar: an old-syntax guard table.
/// `Open` has no timeout and is not allowed to wait forever, a liveness gap.
const LEGACY_TASK_IOA: &str = r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"
guard = [{ type = "is_true", var = "ready" }]
"#;

const TENANT: &str = "default";

/// `Draft`: the same old-syntax spec under another name.
fn legacy_draft_ioa() -> String {
    LEGACY_TASK_IOA.replace("name = \"Task\"", "name = \"Draft\"")
}

/// A CSDL for `entity_type` that declares `Id`, a name the kernel owns.
fn csdl_xml_for(entity_type: &str, set_name: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
    <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
      <edmx:DataServices>
        <Schema Namespace="Temper.Convert" xmlns="http://docs.oasis-open.org/odata/ns/edm">
          <EntityType Name="{entity_type}">
            <Key><PropertyRef Name="Id"/></Key>
            <Property Name="Id" Type="Edm.String" Nullable="false"/>
          </EntityType>
          <EntityContainer Name="ConvertService">
            <EntitySet Name="{set_name}" EntityType="Temper.Convert.{entity_type}"/>
          </EntityContainer>
        </Schema>
      </edmx:DataServices>
    </edmx:Edmx>"#
    )
}

/// A store the server restores its registry from at boot.
enum Store {
    Turso {
        store: TursoEventStore,
        _dir: tempfile::TempDir,
    },
    Postgres(PostgresEventStore),
}

impl Store {
    async fn turso() -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let url = format!("file:{}", dir.path().join("specs.db").display());
        let store = TursoEventStore::new(&url, None)
            .await
            .expect("open the Turso store");
        Self::Turso { store, _dir: dir }
    }

    async fn postgres() -> Self {
        let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
        let options: PgConnectOptions = url.parse().expect("a Postgres URL");
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(options.options([("search_path", "pg_catalog")]))
            .await
            .expect("connect to Postgres");
        let name = format!(
            "spec_conversion_{}",
            temper_runtime::scheduler::sim_uuid().simple()
        );
        let schema = PostgresSchema::new(name).expect("a schema name");
        migration::run_migrations_in_schema(&pool, &schema)
            .await
            .expect("migrate the schema");
        Self::Postgres(PostgresEventStore::with_schema(pool, schema))
    }

    fn platform(&self) -> &dyn PlatformStore {
        match self {
            Self::Turso { store, .. } => store,
            Self::Postgres(store) => store,
        }
    }

    /// Restore a fresh registry, as a boot does.
    async fn boot(&self) -> SpecRegistry {
        let mut registry = SpecRegistry::new();
        let restored = match self {
            Self::Turso { store, .. } => restore_registry_from_turso(&mut registry, store).await,
            Self::Postgres(store) => {
                restore_registry_from_postgres_store(&mut registry, store).await
            }
        }
        .expect("restore the registry");
        assert_eq!(restored, 3, "every stored spec restores");
        registry
    }

    /// The stored source of each type.
    async fn sources(&self) -> BTreeMap<String, String> {
        self.platform()
            .load_specs()
            .await
            .expect("load the stored specs")
            .into_iter()
            .map(|row| (row.entity_type, row.ioa_source))
            .collect()
    }

    /// The source each converted type was stored with before its conversion.
    async fn originals(&self) -> BTreeMap<String, String> {
        self.platform()
            .load_spec_originals()
            .await
            .expect("load the kept originals")
            .into_iter()
            .map(|row| (row.entity_type, row.ioa_source))
            .collect()
    }
}

/// Store `Order` in the current syntax, and `Task` (verified) and `Draft`
/// (not verified) in the old one, all committed.
async fn seed(store: &Store) {
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let draft_ioa = legacy_draft_ioa();
    let platform = store.platform();
    for (entity_type, ioa, set) in [
        ("Order", order_ioa, "Orders"),
        ("Task", LEGACY_TASK_IOA, "Tasks"),
        ("Draft", draft_ioa.as_str(), "Drafts"),
    ] {
        let hash = temper_store_turso::spec_content_hash(ioa);
        platform
            .upsert_spec(
                TENANT,
                entity_type,
                ioa,
                &csdl_xml_for(entity_type, set),
                &hash,
            )
            .await
            .expect("store a spec");
    }
    platform
        .commit_specs(TENANT)
        .await
        .expect("commit the specs");
    platform
        .persist_spec_verification(
            TENANT,
            "Task",
            SpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .expect("mark Task verified");
}

/// Counts events at warn or above logged on this thread.
#[derive(Clone, Default)]
struct Warnings(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> Layer<S> for Warnings {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if *event.metadata().level() <= tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Warnings {
    /// The events counted since the last take.
    fn take(&self) -> usize {
        self.0.swap(0, Ordering::Relaxed)
    }
}

fn verified(registry: &SpecRegistry, entity_type: &str) -> bool {
    match registry.get_verification_status(&TenantId::new(TENANT), entity_type) {
        Some(VerificationStatus::Restored(result) | VerificationStatus::Completed(result)) => {
            result.all_passed
        }
        _ => false,
    }
}

async fn boots_convert_a_stored_spec_once(store: Store) {
    seed(&store).await;
    let warnings = Warnings::default();
    let _logs = tracing::subscriber::set_default(Registry::default().with(warnings.clone()));

    let first = store.boot().await;
    let mut logged = warnings.take();
    let after_first = store.sources().await;
    for entity_type in ["Task", "Draft"] {
        assert!(
            temper_spec::automaton::parse_automaton_with_liveness(
                &after_first[entity_type],
                temper_spec::automaton::LivenessEnforcement::WarnOnly,
            )
            .is_ok(),
            "the first boot converted {entity_type} and kept the old syntax in the store, \
             so every later boot converts it again"
        );
    }
    assert!(verified(&first, "Task") && !verified(&first, "Draft"));
    let kept = store.originals().await;
    for (entity_type, stored) in [
        ("Task", LEGACY_TASK_IOA.to_string()),
        ("Draft", legacy_draft_ioa()),
    ] {
        assert_eq!(
            kept.get(entity_type),
            Some(&stored),
            "the first boot converted {entity_type} and did not keep the text it was \
             stored with, byte for byte, so the conversion cannot be undone"
        );
    }
    assert!(
        !kept.contains_key("Order"),
        "a spec stored in the current syntax keeps nothing extra"
    );
    warnings.take();

    let second = store.boot().await;
    logged += warnings.take();
    assert_eq!(
        store.sources().await,
        after_first,
        "a later boot leaves the stored specs as the first left them"
    );
    assert_eq!(
        store.originals().await,
        kept,
        "a later boot leaves the kept originals as the first left them"
    );
    assert!(
        verified(&second, "Task"),
        "a spec verified as stored stays verified as converted"
    );
    assert!(!verified(&second, "Draft"));
    for entity_type in ["Order", "Task", "Draft"] {
        assert!(
            second
                .get_table(&TenantId::new(TENANT), entity_type)
                .is_some(),
            "{entity_type} restores"
        );
    }
    assert_eq!(
        logged, 0,
        "restoring stored specs logs nothing at warn: an install already reported \
         what a stored spec lacks"
    );

    // Only the first original is kept: a later one changes nothing.
    store
        .platform()
        .keep_spec_original(TENANT, "Task", &after_first["Task"])
        .await
        .expect("keep an original again");
    assert_eq!(
        store.originals().await,
        kept,
        "a kept original is never replaced"
    );
}

#[tokio::test]
async fn a_turso_boot_converts_a_stored_spec_once() {
    boots_convert_a_stored_spec_once(Store::turso().await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn a_postgres_boot_converts_a_stored_spec_once() {
    boots_convert_a_stored_spec_once(Store::postgres().await).await;
}
