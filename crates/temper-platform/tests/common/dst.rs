use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

use super::specs::{
    PROJECT_IOA, catalog_table_arc, collaborator_table_arc, project_table_arc, tenant_table_arc,
    version_table_arc,
};

pub fn new_sim(
    seed: u64,
    max_ticks: u64,
    faults: FaultConfig,
    max_actions_per_actor: usize,
) -> SimActorSystem {
    SimActorSystem::new(SimActorSystemConfig {
        seed,
        max_ticks,
        faults,
        max_actions_per_actor,
    })
}

pub fn register_project(sim: &mut SimActorSystem, id: &str) {
    let handler = EntityActorHandler::new("Project", id.to_string(), project_table_arc())
        .with_ioa_invariants(PROJECT_IOA);
    sim.register_actor(id, Box::new(handler));
}

pub fn register_tenant(sim: &mut SimActorSystem, id: &str) {
    let handler = EntityActorHandler::new("Tenant", id.to_string(), tenant_table_arc());
    sim.register_actor(id, Box::new(handler));
}

pub fn register_catalog_entry(sim: &mut SimActorSystem, id: &str) {
    let handler = EntityActorHandler::new("CatalogEntry", id.to_string(), catalog_table_arc());
    sim.register_actor(id, Box::new(handler));
}

pub fn register_collaborator(sim: &mut SimActorSystem, id: &str) {
    let handler = EntityActorHandler::new("Collaborator", id.to_string(), collaborator_table_arc());
    sim.register_actor(id, Box::new(handler));
}

pub fn register_version(sim: &mut SimActorSystem, id: &str) {
    let handler = EntityActorHandler::new("Version", id.to_string(), version_table_arc());
    sim.register_actor(id, Box::new(handler));
}

pub fn register_projects(sim: &mut SimActorSystem, count: usize) {
    for i in 0..count {
        register_project(sim, &format!("p-{i}"));
    }
}

pub fn register_tenants(sim: &mut SimActorSystem, count: usize) {
    for i in 0..count {
        register_tenant(sim, &format!("t-{i}"));
    }
}

pub fn register_catalog_entries(sim: &mut SimActorSystem, count: usize) {
    for i in 0..count {
        register_catalog_entry(sim, &format!("cat-{i}"));
    }
}

pub fn register_collaborators(sim: &mut SimActorSystem, count: usize) {
    for i in 0..count {
        register_collaborator(sim, &format!("col-{i}"));
    }
}

pub fn register_versions(sim: &mut SimActorSystem, count: usize) {
    for i in 0..count {
        register_version(sim, &format!("v-{i}"));
    }
}

pub fn register_all_system_entities(sim: &mut SimActorSystem) {
    register_project(sim, "p1");
    register_tenant(sim, "t1");
    register_catalog_entry(sim, "cat1");
    register_collaborator(sim, "col1");
    register_version(sim, "v1");
}
