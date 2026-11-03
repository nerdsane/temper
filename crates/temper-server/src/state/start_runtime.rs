//! The one call every embedder makes to start a server's background work.

use std::sync::atomic::Ordering;

use super::{BootTimers, ServerState};

impl ServerState {
    /// Start this server's background runtime. Every embedder (`temper serve`,
    /// temperpaw) calls it once at boot, after its specs, Cedar policies and
    /// WASM modules are loaded and its apps installed, and before requests can
    /// act on records: a timer re-armed here may fire at once.
    ///
    /// It repairs catalog rows behind their journals for every registered tenant,
    /// then restores saved timers and reconciles repaired records through the boot
    /// fire gate before returning. It starts runtime metrics and the background
    /// carries that the installed versions hold for stored records
    /// (`spec_change::carry_pass`). Returns the background walk that recovers
    /// missing timers from the recorded state clock;
    /// it ends with the counts per tenant and type, which it also logs as
    /// `state timers after boot`. A second call in the same process starts
    /// nothing and returns None: a second re-arm would arm every row again.
    #[tracing::instrument(skip_all, fields(otel.name = "boot.start_runtime"))]
    pub async fn start_runtime(&self) -> Option<tokio::task::JoinHandle<BootTimers>> {
        if self.runtime_started.swap(true, Ordering::SeqCst) {
            tracing::warn!("server runtime already started in this process; not started again");
            return None;
        }
        self.spawn_runtime_metrics_loop();
        let boot = self.rearm_state_timers().await;
        // Report once the boot's timers are armed, even when none are, and
        // from now on make timers wait again by the wall clock after a resume.
        self.due_times.start();
        self.spawn_carry_stranded(self.tenants_with_carries());
        Some(boot)
    }
}
