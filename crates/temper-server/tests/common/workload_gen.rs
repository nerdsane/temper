//! Randomized platform workload generator for DST.
//!
//! Generates a sequence of [`WorkloadOp`] values from a deterministic seed.
//! The operations exercise the platform's install/dispatch/restart pipeline
//! without requiring knowledge of exact valid transitions — failed dispatches
//! are expected and exercise error paths in the platform layer.
#![allow(dead_code)]

use std::collections::BTreeMap;

use temper_store_sim::DeterministicRng;

// ── Operation type ──────────────────────────────────────────────────────

/// A single operation in a randomized platform workload.
#[derive(Debug, Clone)]
pub enum WorkloadOp {
    /// Install an OS app for a tenant.
    InstallApp { tenant: String, app: String },
    /// Dispatch an action to an entity.
    Dispatch {
        tenant: String,
        entity_type: String,
        entity_id: String,
        action: String,
    },
    /// Simulate a platform restart.
    Restart,
    /// Check all platform invariants.
    CheckInvariants,
}

// ── Constants ───────────────────────────────────────────────────────────

const TENANTS: &[&str] = &["t-alpha", "t-beta"];

const APPS: &[&str] = &["project-management", "temper-fs", "agent-orchestration"];

/// Entity types provided by each OS app.
fn app_entity_types(app: &str) -> &'static [&'static str] {
    match app {
        "project-management" => &["Issue", "Project", "Cycle", "Comment", "Label"],
        "temper-fs" => &["File", "Directory", "FileVersion", "Workspace"],
        "agent-orchestration" => &["HeartbeatRun", "Organization", "BudgetLedger"],
        _ => &[],
    }
}

/// Common action names per entity type. The generator does not need to know
/// exact valid actions — failed dispatches exercise the platform error path.
fn entity_actions(entity_type: &str) -> &'static [&'static str] {
    match entity_type {
        "Issue" => &[
            "SetDescription",
            "Assign",
            "Archive",
            "SetPriority",
            "AddLabel",
        ],
        "Project" => &["SetDescription", "Archive", "SetLead"],
        "Cycle" => &["SetDescription", "Start", "Complete"],
        "Comment" => &["SetBody", "Archive"],
        "Label" => &["SetName", "SetColor", "Archive"],
        "File" => &["SetDescription", "Upload", "Archive", "Rename"],
        "Directory" => &["SetDescription", "Rename", "Archive"],
        "FileVersion" => &["SetDescription", "Promote"],
        "Workspace" => &["SetDescription", "Archive", "SetOwner"],
        "HeartbeatRun" => &["SetDescription", "Start", "Complete", "Fail"],
        "Organization" => &["SetDescription", "SetName", "Archive"],
        "BudgetLedger" => &["SetDescription", "Credit", "Debit", "Freeze"],
        _ => &["SetDescription"],
    }
}

// ── Generator ───────────────────────────────────────────────────────────

/// Deterministic workload generator.
///
/// Tracks which apps have been installed per tenant and which entity IDs
/// have been created, so that `Dispatch` operations can target existing
/// entities as well as new ones.
pub struct WorkloadGenerator {
    rng: DeterministicRng,
    /// Installed apps per tenant: tenant -> set of app names.
    installed_apps: BTreeMap<String, Vec<String>>,
    /// Known entity IDs per (tenant, entity_type).
    known_entities: BTreeMap<(String, String), Vec<String>>,
    /// Counter for generating unique entity IDs.
    entity_counter: u64,
}

impl WorkloadGenerator {
    /// Create a new generator with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: DeterministicRng::new(seed.wrapping_add(0x_CAFE_BABE)),
            installed_apps: BTreeMap::new(),
            known_entities: BTreeMap::new(),
            entity_counter: 0,
        }
    }

    /// Generate the next operation.
    ///
    /// Distribution:
    /// -  10% InstallApp
    /// -  50% Dispatch (requires at least one installed app)
    /// -  25% Restart
    /// -  15% CheckInvariants
    pub fn next_op(&mut self) -> WorkloadOp {
        let roll = self.rng.next_u64() % 100;

        match roll {
            // 10% InstallApp
            0..10 => self.gen_install_app(),
            // 50% Dispatch — but only if we have installed apps
            10..60 => {
                if self.has_installed_apps() {
                    self.gen_dispatch()
                } else {
                    // No apps installed yet — fall back to InstallApp.
                    self.gen_install_app()
                }
            }
            // 25% Restart
            60..85 => WorkloadOp::Restart,
            // 15% CheckInvariants
            _ => WorkloadOp::CheckInvariants,
        }
    }

    /// Record that an app was successfully installed (called by the test
    /// runner after a successful `install_skill`).
    pub fn record_install(&mut self, tenant: &str, app: &str) {
        let apps = self.installed_apps.entry(tenant.to_string()).or_default();
        if !apps.contains(&app.to_string()) {
            apps.push(app.to_string());
        }
    }

    // ── Private helpers ─────────────────────────────────────────────

    fn has_installed_apps(&self) -> bool {
        self.installed_apps.values().any(|apps| !apps.is_empty())
    }

    fn pick<'a>(&mut self, items: &'a [&str]) -> &'a str {
        let idx = (self.rng.next_u64() as usize) % items.len();
        items[idx]
    }

    fn pick_string(&mut self, items: &[String]) -> String {
        let idx = (self.rng.next_u64() as usize) % items.len();
        items[idx].clone()
    }

    fn gen_install_app(&mut self) -> WorkloadOp {
        let tenant = self.pick(TENANTS).to_string();
        let app = self.pick(APPS).to_string();
        WorkloadOp::InstallApp { tenant, app }
    }

    fn gen_dispatch(&mut self) -> WorkloadOp {
        // Pick a random tenant that has installed apps.
        let tenants_with_apps: Vec<String> = self
            .installed_apps
            .iter()
            .filter(|(_, apps)| !apps.is_empty())
            .map(|(t, _)| t.clone())
            .collect();
        let tenant = self.pick_string(&tenants_with_apps);

        // Pick a random app installed on this tenant.
        let apps = &self.installed_apps[&tenant];
        let app_idx = (self.rng.next_u64() as usize) % apps.len();
        let app = &apps[app_idx];

        // Pick a random entity type from this app.
        let entity_types = app_entity_types(app);
        let et_idx = (self.rng.next_u64() as usize) % entity_types.len();
        let entity_type = entity_types[et_idx].to_string();

        // Pick or create an entity ID (50/50).
        let key = (tenant.clone(), entity_type.clone());
        let entity_id = if self.rng.chance(0.5) {
            // Try to use an existing entity.
            if let Some(ids) = self.known_entities.get(&key) {
                if !ids.is_empty() {
                    let id_idx = (self.rng.next_u64() as usize) % ids.len();
                    ids[id_idx].clone()
                } else {
                    self.new_entity_id(&key)
                }
            } else {
                self.new_entity_id(&key)
            }
        } else {
            self.new_entity_id(&key)
        };

        // Pick a random action.
        let actions = entity_actions(&entity_type);
        let act_idx = (self.rng.next_u64() as usize) % actions.len();
        let action = actions[act_idx].to_string();

        WorkloadOp::Dispatch {
            tenant,
            entity_type,
            entity_id,
            action,
        }
    }

    fn new_entity_id(&mut self, key: &(String, String)) -> String {
        self.entity_counter += 1;
        let id = format!("e-{}", self.entity_counter);
        self.known_entities
            .entry(key.clone())
            .or_default()
            .push(id.clone());
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generator_produces_ops() {
        let mut wg = WorkloadGenerator::new(42);
        // First ops should be InstallApp (no apps installed yet, dispatch
        // falls back to install).
        for _ in 0..10 {
            let op = wg.next_op();
            match &op {
                WorkloadOp::InstallApp { tenant, app } => {
                    wg.record_install(tenant, app);
                }
                WorkloadOp::Restart | WorkloadOp::CheckInvariants => {}
                WorkloadOp::Dispatch { .. } => {
                    // Should not happen before any install, but if it does
                    // that's fine — generator falls back to install.
                }
            }
        }
        // After several installs, we should see dispatches.
        let mut saw_dispatch = false;
        for _ in 0..100 {
            let op = wg.next_op();
            if let WorkloadOp::Dispatch { .. } = &op {
                saw_dispatch = true;
            }
            if let WorkloadOp::InstallApp { tenant, app } = &op {
                wg.record_install(tenant, app);
            }
        }
        assert!(saw_dispatch, "expected at least one Dispatch op");
    }

    #[test]
    fn generator_is_deterministic() {
        let ops_a: Vec<String> = {
            let mut wg = WorkloadGenerator::new(99);
            (0..50).map(|_| format!("{:?}", wg.next_op())).collect()
        };
        let ops_b: Vec<String> = {
            let mut wg = WorkloadGenerator::new(99);
            (0..50).map(|_| format!("{:?}", wg.next_op())).collect()
        };
        assert_eq!(ops_a, ops_b);
    }
}
