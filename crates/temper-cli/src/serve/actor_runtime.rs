//! Actor runtime startup wiring for `temper serve`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use deadpool_postgres::{Config as PgConfig, Runtime};
use tokio::sync::watch;
use tokio_postgres::NoTls;

use temper_actor_runtime::{ActorSystem as PgActorSystem, SchedulerConfig, SpecDrivenActor};
use temper_runtime::reaction::{
    ReactionRegistry, ReactionRule as ActorReactionRule, ReactionTarget as ActorReactionTarget,
    ReactionTrigger as ActorReactionTrigger, TargetResolver as ActorTargetResolver,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntitySpec, SpecRegistry};
use temper_server::state::ServerState;
use temper_server::trigger::{ReactionRule as ServerReactionRule, TargetResolver};

use crate::StorageBackend;

pub(super) struct ConfiguredPostgresActorRuntime {
    pub system: Arc<PgActorSystem>,
    pub actor_backed_types: BTreeSet<String>,
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
    reaction_rules: Vec<ActorReactionRule>,
}

#[derive(Debug)]
struct ActorRuntimeDefinitions {
    definitions: Vec<ActorRuntimeDefinition>,
    actor_backed_keys: BTreeSet<String>,
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
        let actor = SpecDrivenActor::from_ioa(
            &definition.ioa_source,
            ReactionRegistry::from(definition.reaction_rules),
        )
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

    let mut client = pool
        .get()
        .await
        .context("failed to get Postgres actor-runtime client")?;
    temper_actor_runtime::schema::create_tables(&mut client)
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
    let mut actor_backed_keys = BTreeSet::new();
    for tenant in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant) {
            if !selected.matches(tenant.as_str(), entity_type) {
                continue;
            }
            let spec = registry.get_spec(tenant, entity_type).ok_or_else(|| {
                anyhow!("missing registry spec for tenant {tenant} entity {entity_type}")
            })?;
            validate_actor_runtime_compatible(tenant, entity_type, spec)?;
            let reaction_rules = actor_reaction_rules(registry, tenant, entity_type, &selected)?;

            match definitions.get(entity_type) {
                Some(existing)
                    if existing.ioa_source != spec.ioa_source
                        || existing.reaction_rules != reaction_rules =>
                {
                    bail!(
                        "actor-backed entity type {entity_type:?} has different IOA or reaction definitions across tenants; the current Postgres actor runtime supports one handler per actor type"
                    )
                }
                Some(_) => {}
                None => {
                    definitions.insert(
                        entity_type.to_string(),
                        ActorRuntimeDefinition {
                            entity_type: entity_type.to_string(),
                            ioa_source: spec.ioa_source.clone(),
                            reaction_rules,
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

    Ok(())
}

fn actor_reaction_rules(
    registry: &SpecRegistry,
    tenant: &TenantId,
    entity_type: &str,
    selected: &ActorBackedSelection,
) -> Result<Vec<ActorReactionRule>> {
    let mut converted = Vec::new();
    for rule in registry
        .reaction_rules_for_tenant(tenant)
        .into_iter()
        .filter(|rule| rule.when.entity_type == entity_type)
    {
        let target_type = &rule.then.entity_type;
        if registry.get_spec(tenant, target_type).is_none()
            || !selected.matches(tenant.as_str(), target_type)
        {
            bail!(
                "tenant {tenant} reaction {:?} targets actor type {target_type:?}, which must be loaded and selected for --actor-runtime postgres",
                rule.name
            );
        }
        converted.push(actor_reaction_rule(tenant, &rule)?);
    }
    Ok(converted)
}

fn actor_reaction_rule(tenant: &TenantId, rule: &ServerReactionRule) -> Result<ActorReactionRule> {
    if rule.when.guard.is_some()
        || rule.principal.is_some()
        || !rule.then.params_from.is_empty()
        || !empty_params(&rule.then.params)
    {
        bail!(
            "tenant {tenant} reaction {:?} uses guarded, principal, or parameter mapping semantics that --actor-runtime postgres cannot preserve",
            rule.name
        );
    }
    let resolve_target = match &rule.resolve_target {
        TargetResolver::SameId => ActorTargetResolver::SameId,
        TargetResolver::Field { field } => ActorTargetResolver::Field {
            field: field.clone(),
        },
        TargetResolver::Static { entity_id } => ActorTargetResolver::Static {
            entity_id: entity_id.clone(),
        },
        TargetResolver::CreateIfMissing { id_field } => ActorTargetResolver::CreateIfMissing {
            id_field: id_field.clone(),
        },
        TargetResolver::Create => bail!(
            "tenant {tenant} reaction {:?} uses create target resolution, which --actor-runtime postgres cannot preserve",
            rule.name
        ),
    };
    Ok(ActorReactionRule {
        name: rule.name.clone(),
        when: ActorReactionTrigger {
            entity_type: rule.when.entity_type.clone(),
            action: rule.when.action.clone(),
            to_state: rule.when.to_state.clone(),
        },
        then: ActorReactionTarget {
            entity_type: rule.then.entity_type.clone(),
            action: rule.then.action.clone(),
        },
        resolve_target,
    })
}

fn empty_params(params: &serde_json::Value) -> bool {
    params.is_null() || params.as_object().is_some_and(serde_json::Map::is_empty)
}

#[cfg(test)]
#[path = "actor_runtime/tests.rs"]
mod tests;
