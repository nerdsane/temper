//! The carries of each installed version, made after the version serves.
//!
//! An install checks a new version and tries its carries before the swap
//! (see the module above), then answers and serves. The carries run after it
//! in the background, as ordinary journaled moves of the platform. Until its
//! carry is made a record waits in its old state, where no caller move is
//! possible for it, so how many records a version carries does not change how
//! long an install or a start takes.
//!
//! A pass derives its carries again from the installed versions each time it
//! runs: a carry applies to each record in its `from` state that the version
//! leaves with no way forward and that the carry's guard selects, and is tried
//! with the move code before it is made. A pass runs after each install into a
//! tenant, and at boot once the runtime has started, so a pass that a restart
//! cut short is finished at the next start and a carried record is not moved
//! again. A carry that cannot be made is logged and its record stays where it
//! is.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_jit::swap::SwapController;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;

use super::then_move::then_maker;
use super::{CarryMove, Checked, SpecChange, SpecChangePlan, SpecChangeRefused};
use crate::entity_actor::effects::entity_authorization_precondition;
use crate::request_context::AgentContext;
use crate::state::redecide::Redecide;
use crate::state::{DispatchCommand, ServerState};

/// The version a type is served from: the controller that swaps its table,
/// and how many swaps it has made.
type Served = (Arc<SwapController>, u64);

/// What one carry pass did in one tenant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarryReport {
    /// Records carried.
    pub carried: usize,
    /// Why each carry that applies was not made.
    pub failed: Vec<String>,
    /// Carried records whose `then` move is not made, since the field its
    /// `as` names is empty.
    pub left: Vec<String>,
}

impl ServerState {
    /// Make each carry of `tenant`'s installed versions on the records it
    /// applies to now. Passes run one at a time, so each derives its carries
    /// after the one before it made its own. Installs and the boot run it with
    /// [`Self::spawn_carry_stranded`].
    pub async fn carry_stranded(&self, tenant: &TenantId) -> CarryReport {
        let _pass = self.carry_pass.lock().await;
        let installed: Vec<(SpecChange, Arc<TransitionTable>, Served)> = {
            let registry = self.registry.read().expect("registry lock poisoned");
            registry
                .entity_types(tenant)
                .into_iter()
                .filter_map(|entity_type| {
                    let spec = registry.get_spec(tenant, entity_type)?;
                    (!spec.automaton.carries.is_empty()).then(|| {
                        let change = SpecChange {
                            entity_type: entity_type.to_string(),
                            proposed: spec.automaton.clone(),
                        };
                        let swap = spec.swap_controller();
                        (change, spec.table(), (Arc::clone(swap), swap.version()))
                    })
                })
                .collect()
        };
        let mut plan = SpecChangePlan::default();
        let mut refused = SpecChangeRefused::default();
        let mut planned: BTreeMap<&str, &Served> = BTreeMap::new();
        for (change, table, served) in &installed {
            let checked = Checked {
                tables: (table, table),
                cedar: &self.authz,
            };
            self.check_type(tenant, change, checked, &mut plan, &mut refused)
                .await;
            planned.insert(&change.entity_type, served);
        }
        let mut report = CarryReport {
            carried: 0,
            failed: refused.reasons,
            left: Vec::new(),
        };
        for carry in &plan.carries {
            // A carry's values are its move's parameters, so it is made only
            // under the version that planned it. An install that swapped the
            // type since starts its own pass, which waits for this one and
            // plans from the version it installed.
            if !self.still_serves(
                tenant,
                &carry.entity_type,
                planned[carry.entity_type.as_str()],
            ) {
                tracing::info!(tenant = %tenant, entity_type = %carry.entity_type,
                    "a newer version was installed during the carry pass; its pass makes the rest");
                break;
            }
            if let Err(error) = self.make_carry(tenant, carry).await {
                report.failed.push(error);
                continue;
            }
            report.carried += 1;
            let Some(then) = &carry.then else {
                continue;
            };
            match self.make_then(tenant, carry, then).await {
                Ok(true) => {}
                Ok(false) => report.left.push(format!(
                    "{} {}: carried; its then move {then} is not made, since its `{}` is empty",
                    carry.entity_type,
                    carry.entity_id,
                    carry.acts_for.as_deref().unwrap_or_default()
                )),
                Err(error) => report.failed.push(format!(
                    "{} {}: carried, but its then move {then} failed: {error}",
                    carry.entity_type, carry.entity_id
                )),
            }
        }
        if !plan.carries.is_empty() || !report.failed.is_empty() {
            tracing::info!(tenant = %tenant, carried = report.carried,
                failed = report.failed.len(),
                "carried stored records to their installed spec versions");
        }
        for failure in &report.failed {
            tracing::warn!(tenant = %tenant, %failure, "a carry was not made");
        }
        for left in &report.left {
            tracing::info!(tenant = %tenant, %left, "a carried record has no one to act for");
        }
        report
    }

    /// [`Self::carry_stranded`] for each of `tenants`, in the background, once
    /// this server's runtime has started. Before that a pass would compete
    /// with the start it follows and arm timers ahead of the boot's re-arm, so
    /// [`Self::start_runtime`] starts the pass for every tenant itself.
    pub fn spawn_carry_stranded(&self, tenants: Vec<TenantId>) {
        if tenants.is_empty()
            || !self
                .timer_recovery_complete
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let server = self.clone();
        // determinism-ok: carries run in the background, after the version serves
        runtime.spawn(async move {
            for tenant in &tenants {
                server.carry_stranded(tenant).await;
            }
        });
    }

    /// The tenants with an installed version that holds a `[[carry]]`.
    pub(crate) fn tenants_with_carries(&self) -> Vec<TenantId> {
        let registry = self.registry.read().expect("registry lock poisoned");
        registry
            .tenant_ids()
            .into_iter()
            .filter(|tenant| {
                registry
                    .entity_types(tenant)
                    .into_iter()
                    .any(|entity_type| {
                        registry
                            .get_spec(tenant, entity_type)
                            .is_some_and(|spec| !spec.automaton.carries.is_empty())
                    })
            })
            .cloned()
            .collect()
    }

    /// Whether `tenant` still serves `entity_type` from the version `served`
    /// names.
    fn still_serves(&self, tenant: &TenantId, entity_type: &str, served: &Served) -> bool {
        let registry = self.registry.read().expect("registry lock poisoned");
        registry.get_spec(tenant, entity_type).is_some_and(|spec| {
            let swap = spec.swap_controller();
            Arc::ptr_eq(swap, &served.0) && swap.version() == served.1
        })
    }

    /// Make one carry as a move of the platform.
    async fn make_carry(&self, tenant: &TenantId, carry: &CarryMove) -> Result<(), String> {
        let result = self
            .dispatch_tenant_action_core(
                tenant,
                &carry.entity_type,
                &carry.entity_id,
                &carry.action,
                carry.params.clone(),
                &AgentContext::system(),
                false,
                crate::state::write_record::WriteCaller::Kernel,
                None,
            )
            .await;
        let error = match result {
            Ok(response) if response.success => return Ok(()),
            Ok(response) => response.error.unwrap_or_default(),
            Err(error) => error.to_string(),
        };
        Err(format!(
            "{} {}: {} failed: {error}",
            carry.entity_type, carry.entity_id, carry.action
        ))
    }

    /// Make a carry's `then` move on the record it carried, as any move is
    /// made: decided by the version's Cedar on the record as it is now (and
    /// again if the record changes before the move reaches it), with
    /// its effects, triggers and reactions, by [`then_maker`]. A carry itself
    /// is a system move, and a module gets no capability from a system move;
    /// the modules this move's triggers call get the capability any move's
    /// modules get. `false` when the record's `as` field is empty, so the
    /// move is not made.
    async fn make_then(
        &self,
        tenant: &TenantId,
        carry: &CarryMove,
        then: &str,
    ) -> Result<bool, String> {
        let mut redecide = Redecide::start(self);
        loop {
            let snapshot = self
                .load_authz_resource_snapshot(tenant, &carry.entity_type, &carry.entity_id)
                .await
                .map_err(|error| error.to_string())?;
            let Some(agent_ctx) = then_maker(
                &carry.action,
                carry.acts_for.as_deref(),
                &snapshot.current_state.state.fields,
            ) else {
                return Ok(false);
            };
            let security_ctx = agent_ctx
                .security_ctx
                .as_ref()
                .expect("a then move's maker carries its security context");
            self.authorize_with_context(
                security_ctx,
                then,
                &carry.entity_type,
                &snapshot.resource_attrs,
                tenant.as_str(),
            )
            .map_err(|denial| denial.to_string())?;
            let decided_on = entity_authorization_precondition(&snapshot.current_state.state);
            redecide.dispatching().await;
            let response = self
                .dispatch_tenant_action_ext_typed_if_current(
                    DispatchCommand {
                        tenant,
                        entity_type: &carry.entity_type,
                        entity_id: &carry.entity_id,
                        action: then,
                        params: serde_json::json!({}),
                        agent_ctx: &agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    },
                    crate::state::write_record::WriteCaller::Kernel,
                    decided_on,
                )
                .await
                .map_err(|error| error.to_string())?;
            if redecide.again(response.is_record_changed(), "carry_then") {
                continue;
            }
            return match response.success {
                true => Ok(true),
                false => Err(response.error.unwrap_or_default()),
            };
        }
    }
}
