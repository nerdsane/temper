//! Actor runtime startup wiring for `temper serve`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use deadpool_postgres::{Config as PgConfig, Runtime};
use tokio::sync::watch;
use tokio_postgres::NoTls;

use temper_actor_runtime::{ActorSystem as PgActorSystem, SchedulerConfig, SpecDrivenActor};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntitySpec, SpecRegistry};
use temper_server::state::ServerState;
use temper_spec::automaton::Effect;

use crate::StorageBackend;

pub(super) struct ConfiguredPostgresActorRuntime {
    pub system: Arc<PgActorSystem>,
    pub actor_backed_types: HashSet<String>,
    pub cancel: watch::Sender<bool>,
}

pub(super) async fn install_postgres_actor_runtime(
    storage: StorageBackend,
    postgres_storage_active: bool,
    raw_actor_backed_types: &[String],
    server_state: &mut ServerState,
) -> Result<watch::Sender<bool>> {
    let configured = configure_postgres_actor_runtime(
        storage,
        postgres_storage_active,
        raw_actor_backed_types,
        &server_state.registry,
    )
    .await?;
    server_state.pg_actor_system = Some(configured.system.clone());
    server_state.actor_backed_types = configured.actor_backed_types;
    Ok(configured.cancel)
}

#[derive(Debug)]
struct ActorRuntimeDefinition {
    entity_type: String,
    ioa_source: String,
}

#[derive(Debug)]
struct ActorRuntimeDefinitions {
    definitions: Vec<ActorRuntimeDefinition>,
    actor_backed_keys: HashSet<String>,
}

#[derive(Debug, Default)]
struct ActorBackedSelection {
    all: bool,
    global_types: BTreeSet<String>,
    tenant_types: BTreeSet<(String, String)>,
}

pub(super) async fn configure_postgres_actor_runtime(
    storage: StorageBackend,
    postgres_storage_active: bool,
    raw_actor_backed_types: &[String],
    registry: &Arc<RwLock<SpecRegistry>>,
) -> Result<ConfiguredPostgresActorRuntime> {
    if storage != StorageBackend::Postgres || !postgres_storage_active {
        bail!(
            "--actor-runtime postgres requires --storage postgres or TEMPER_EVENT_STORE=postgres with DATABASE_URL configured"
        );
    }

    let database_url = std::env::var("DATABASE_URL")
        .context("DATABASE_URL is required when --actor-runtime postgres is selected")?;
    let actor_pool = connect_actor_pool(&database_url).await?;
    let definitions = {
        let registry = registry
            .read()
            .map_err(|_| anyhow!("spec registry lock poisoned"))?;
        collect_actor_runtime_definitions(&registry, raw_actor_backed_types)?
    };
    if definitions.definitions.is_empty() {
        bail!("--actor-runtime postgres selected but no actor-backed entity types are loaded");
    }

    let system = Arc::new(PgActorSystem::new(actor_pool, SchedulerConfig::default()));
    for definition in definitions.definitions {
        let actor = SpecDrivenActor::from_ioa(&definition.ioa_source, HashMap::new())
            .map_err(|e| anyhow!("failed to build actor for {}: {e}", definition.entity_type))?;
        system.register(Arc::new(actor)).await.with_context(|| {
            format!(
                "failed to register postgres actor type {}",
                definition.entity_type
            )
        })?;
    }

    let (cancel, cancel_rx) = watch::channel(false);
    let scheduler_system = system.clone();
    tokio::spawn(async move {
        scheduler_system.run(cancel_rx).await;
    });

    Ok(ConfiguredPostgresActorRuntime {
        system,
        actor_backed_types: definitions.actor_backed_keys,
        cancel,
    })
}

async fn connect_actor_pool(database_url: &str) -> Result<deadpool_postgres::Pool> {
    let mut cfg = PgConfig::new();
    cfg.url = Some(database_url.to_string());
    let pool = cfg
        .create_pool(Some(Runtime::Tokio1), NoTls)
        .context("failed to create Postgres actor-runtime pool")?;

    let client = pool
        .get()
        .await
        .context("failed to get Postgres actor-runtime client")?;
    temper_actor_runtime::schema::create_tables(&client)
        .await
        .context("failed to initialize Postgres actor-runtime tables")?;

    Ok(pool)
}

fn collect_actor_runtime_definitions(
    registry: &SpecRegistry,
    raw_actor_backed_types: &[String],
) -> Result<ActorRuntimeDefinitions> {
    let selected = parse_actor_backed_types(raw_actor_backed_types)?;
    let mut available = BTreeSet::new();
    let mut available_by_tenant = BTreeSet::new();
    for tenant in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant) {
            available.insert(entity_type.to_string());
            available_by_tenant.insert((tenant.as_str().to_string(), entity_type.to_string()));
        }
    }

    if !selected.all {
        for entity_type in &selected.global_types {
            if !available.contains(entity_type.as_str()) {
                bail!(
                    "actor-backed entity type {entity_type:?} is not loaded in the spec registry"
                );
            }
        }
        for (tenant, entity_type) in &selected.tenant_types {
            if !available_by_tenant.contains(&(tenant.clone(), entity_type.clone())) {
                bail!(
                    "actor-backed entity type {tenant}:{entity_type} is not loaded in the spec registry"
                );
            }
        }
    }

    let mut definitions = BTreeMap::<String, ActorRuntimeDefinition>::new();
    let mut actor_backed_keys = HashSet::new();
    for tenant in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant) {
            if !selected.matches(tenant.as_str(), entity_type) {
                continue;
            }
            let spec = registry.get_spec(tenant, entity_type).ok_or_else(|| {
                anyhow!("missing registry spec for tenant {tenant} entity {entity_type}")
            })?;
            validate_actor_runtime_compatible(tenant, entity_type, spec)?;

            match definitions.get(entity_type) {
                Some(existing) if existing.ioa_source != spec.ioa_source => bail!(
                    "actor-backed entity type {entity_type:?} has different IOA sources across tenants; the current Postgres actor runtime supports one handler per actor type"
                ),
                Some(_) => {}
                None => {
                    definitions.insert(
                        entity_type.to_string(),
                        ActorRuntimeDefinition {
                            entity_type: entity_type.to_string(),
                            ioa_source: spec.ioa_source.clone(),
                        },
                    );
                }
            }

            if selected.all || selected.global_types.contains(entity_type) {
                actor_backed_keys.insert(entity_type.to_string());
            } else {
                actor_backed_keys.insert(format!("{}:{entity_type}", tenant.as_str()));
            }
        }
    }

    Ok(ActorRuntimeDefinitions {
        definitions: definitions.into_values().collect(),
        actor_backed_keys,
    })
}

fn parse_actor_backed_types(raw: &[String]) -> Result<ActorBackedSelection> {
    if raw.is_empty() {
        return Ok(ActorBackedSelection {
            all: true,
            ..Default::default()
        });
    }

    let mut selection = ActorBackedSelection::default();
    for entry in raw {
        for token in entry.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if token == "*" || token.eq_ignore_ascii_case("all") {
                selection.all = true;
            } else if let Some((tenant, entity_type)) = token.split_once(':') {
                let tenant = tenant.trim();
                let entity_type = entity_type.trim();
                if tenant.is_empty() || entity_type.is_empty() {
                    bail!("actor-backed type {token:?} must be formatted as tenant:EntityType");
                }
                selection
                    .tenant_types
                    .insert((tenant.to_string(), entity_type.to_string()));
            } else {
                selection.global_types.insert(token.to_string());
            }
        }
    }

    if selection.all && (!selection.global_types.is_empty() || !selection.tenant_types.is_empty()) {
        bail!("--actor-backed-type all cannot be combined with specific entity types");
    }
    if !selection.all && selection.global_types.is_empty() && selection.tenant_types.is_empty() {
        bail!("--actor-backed-type was provided but no entity type names were found");
    }
    Ok(selection)
}

impl ActorBackedSelection {
    fn matches(&self, tenant: &str, entity_type: &str) -> bool {
        self.all
            || self.global_types.contains(entity_type)
            || self
                .tenant_types
                .contains(&(tenant.to_string(), entity_type.to_string()))
    }
}

fn validate_actor_runtime_compatible(
    tenant: &TenantId,
    entity_type: &str,
    spec: &EntitySpec,
) -> Result<()> {
    if !spec.integrations.is_empty() {
        bail!(
            "tenant {tenant} entity {entity_type} declares legacy integrations, which are not yet supported by --actor-runtime postgres"
        );
    }

    for action in &spec.automaton.actions {
        if !action.triggers.is_empty() {
            bail!(
                "tenant {tenant} entity {entity_type} action {} declares action triggers, which are not yet supported by --actor-runtime postgres",
                action.name
            );
        }
        for effect in &action.effect {
            match effect {
                Effect::Increment {
                    amount: Some(_), ..
                }
                | Effect::Decrement {
                    amount: Some(_), ..
                }
                | Effect::SetCounterFromParam { .. }
                | Effect::ListAppend { .. }
                | Effect::ListRemoveAt { .. }
                | Effect::Trigger { .. }
                | Effect::Schedule { .. }
                | Effect::ScheduleAt { .. }
                | Effect::Spawn { .. } => {
                    bail!(
                        "tenant {tenant} entity {entity_type} action {} uses effect {:?}, which is not yet supported by --actor-runtime postgres",
                        action.name,
                        effect
                    );
                }
                Effect::Increment { amount: None, .. }
                | Effect::Decrement { amount: None, .. }
                | Effect::SetBool { .. }
                | Effect::Emit { .. } => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;

    const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
    const PROCESS_IOA: &str = include_str!("../../../../test-fixtures/specs/process.ioa.toml");

    fn registry_with(tenant: &str, entity_type: &str, ioa_source: &str) -> SpecRegistry {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            tenant,
            csdl,
            CSDL_XML.to_string(),
            &[(entity_type, ioa_source)],
        );
        registry
    }

    #[test]
    fn parses_actor_backed_type_tokens() {
        let parsed = parse_actor_backed_types(&["Order,Invoice".into(), "Customer".into()])
            .expect("actor-backed type list should parse");
        assert!(!parsed.all);
        assert!(parsed.global_types.contains("Order"));
        assert!(parsed.global_types.contains("Invoice"));
        assert!(parsed.global_types.contains("Customer"));
    }

    #[test]
    fn parses_tenant_scoped_actor_backed_type_tokens() {
        let parsed = parse_actor_backed_types(&["alpha:Order,beta:Invoice".into()])
            .expect("tenant-scoped actor-backed type list should parse");

        assert!(!parsed.all);
        assert!(
            parsed
                .tenant_types
                .contains(&("alpha".into(), "Order".into()))
        );
        assert!(
            parsed
                .tenant_types
                .contains(&("beta".into(), "Invoice".into()))
        );
    }

    #[test]
    fn parses_all_actor_backed_types() {
        assert!(
            parse_actor_backed_types(&["all".into()])
                .expect("all selector should parse")
                .all
        );
        assert!(
            parse_actor_backed_types(&["*".into()])
                .expect("wildcard selector should parse")
                .all
        );
    }

    #[test]
    fn rejects_all_mixed_with_specific_types() {
        let err = parse_actor_backed_types(&["all".into(), "Order".into()]).unwrap_err();
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[test]
    fn collects_compatible_registry_specs() {
        let registry = registry_with("alpha", "Order", ORDER_IOA);
        let definitions = collect_actor_runtime_definitions(&registry, &[])
            .expect("compatible registry specs should collect");

        assert_eq!(definitions.definitions.len(), 1);
        assert_eq!(definitions.definitions[0].entity_type, "Order");
        assert!(definitions.actor_backed_keys.contains("Order"));
    }

    #[test]
    fn collects_tenant_scoped_registry_specs() {
        let registry = registry_with("alpha", "Order", ORDER_IOA);
        let definitions = collect_actor_runtime_definitions(&registry, &["alpha:Order".into()])
            .expect("tenant-scoped registry specs should collect");

        assert_eq!(definitions.definitions.len(), 1);
        assert_eq!(definitions.definitions[0].entity_type, "Order");
        assert!(definitions.actor_backed_keys.contains("alpha:Order"));
    }

    #[test]
    fn rejects_missing_explicit_actor_backed_type() {
        let registry = registry_with("alpha", "Order", ORDER_IOA);
        let err = collect_actor_runtime_definitions(&registry, &["Invoice".into()]).unwrap_err();

        assert!(err.to_string().contains("is not loaded"));
    }

    #[test]
    fn rejects_unsupported_actor_effects() {
        let registry = registry_with("alpha", "Process", PROCESS_IOA);
        let err = collect_actor_runtime_definitions(&registry, &["Process".into()]).unwrap_err();

        assert!(err.to_string().contains("not yet supported"));
    }

    #[test]
    fn rejects_conflicting_same_named_specs_across_tenants() {
        let mut registry = registry_with("alpha", "Order", ORDER_IOA);
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let beta_ioa = ORDER_IOA.replace("initial = \"Draft\"", "initial = \"Submitted\"");
        registry.register_tenant("beta", csdl, CSDL_XML.to_string(), &[("Order", &beta_ioa)]);

        let err = collect_actor_runtime_definitions(&registry, &["Order".into()]).unwrap_err();

        assert!(err.to_string().contains("different IOA sources"));
    }
}
