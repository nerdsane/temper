//! What an os-app install holds and checks before it writes (nerdsane/temper#418).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::state::TimerFollowHold;
use tokio::sync::{Mutex as TenantLock, OwnedMutexGuard};

/// What an install into one tenant holds until it ends.
pub(crate) struct TenantInstall {
    _serialized: OwnedMutexGuard<()>,
    /// Keeps the tenant's changed specs out of every walk, this server's
    /// other tenants' included, until its policies and modules are in.
    timers: Option<TimerFollowHold>,
    tenant: TenantId,
}

impl TenantInstall {
    /// The install's policies and modules are in: its changed specs may make
    /// records follow their states now, and when `specs` it starts that walk
    /// and the pass that makes the carries its versions hold.
    pub(super) fn installed(&mut self, server: &ServerState, specs: bool) {
        self.timers = None;
        if specs {
            server.spawn_follow_spec_changes();
            server.spawn_carry_stranded(vec![self.tenant.clone()]);
        }
    }
}

/// Serialize installs into one tenant, and hold its changed specs back from
/// every walk until [`TenantInstall::installed`]. Each install reads the
/// tenant's active Cedar, merges its app's statements in and loads the result;
/// two at once would both read the same text, and the second load would drop
/// the first app's statements while both installs answered Ok.
pub(crate) async fn tenant(server: &ServerState, tenant: &str) -> TenantInstall {
    static LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<TenantLock<()>>>>> = OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap() // ci-ok: infallible lock
        .entry(tenant.to_string())
        .or_default()
        .clone();
    let serialized = lock.lock_owned().await;
    let tenant = TenantId::new(tenant);
    TenantInstall {
        _serialized: serialized,
        timers: Some(server.hold_timer_follow(&tenant)),
        tenant,
    }
}

/// Refuse, before anything is written, an install whose tenant Cedar would not
/// compile: going on would persist Cedar the engine then fails to load, and the
/// tenant would keep running on whatever set was live.
pub(super) fn refuse_uncompilable(
    authz: &temper_authz::AuthzEngine,
    app_name: &str,
    tenant: &str,
    combined_policy: Option<&str>,
) -> Result<(), String> {
    // The engine's live text compiled when it was loaded; a reinstall that
    // changes nothing (every boot) need not parse the whole tenant again.
    let live = authz.get_tenant_policy_text(tenant);
    let changed =
        combined_policy.filter(|text| live.as_deref().map(str::trim_end) != Some(text.trim_end()));
    match changed.map(|text| authz.validate_tenant_policies(text)) {
        Some(Err(error)) => Err(format!(
            "Refusing os-app '{app_name}' for tenant '{tenant}': its Cedar does not compile with the tenant's: {error}"
        )),
        _ => Ok(()),
    }
}
