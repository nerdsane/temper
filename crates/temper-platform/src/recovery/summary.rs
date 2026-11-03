//! What runtime recovery found for each installed app, and how it is told.

/// Runtime-only outcome for installed-app warm restart recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledAppRuntimeRecoveryOutcome {
    /// Specs were already registered and marked usable.
    Ready,
    /// Specs were registered and the durable bundle digest matched; runtime
    /// verification readiness was repaired without reinstalling app content.
    Healed,
    /// The app should be reconciled by the digest-aware app reconcile path.
    NeedsReconcile,
    /// The durable installed-app row points at an app that is not in the catalog.
    MissingBundle,
    /// Durable metadata could not be read, so the caller should reconcile.
    StoreError,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstalledAppsRuntimeRecoverySummary {
    pub ready: usize,
    pub healed: usize,
    pub needs_reconcile: usize,
    pub missing_bundle: usize,
    pub store_error: usize,
}

impl InstalledAppsRuntimeRecoverySummary {
    /// Log the summary: at WARN, with `result = "error"`, when an installed
    /// app could not be recovered because its files are not in the catalog or
    /// its record could not be read. Such an app runs only once its files
    /// arrive, so the line is for whoever watches the logs.
    pub(super) fn report(&self) {
        if self.missing_bundle > 0 || self.store_error > 0 {
            tracing::warn!(
                recovery = "installed_apps",
                result = "error",
                ready = self.ready,
                healed = self.healed,
                needs_reconcile = self.needs_reconcile,
                missing_bundle = self.missing_bundle,
                store_error = self.store_error,
                "Installed app runtime recovery left apps it could not restore"
            );
        } else {
            tracing::info!(
                recovery = "installed_apps",
                result = "ok",
                ready = self.ready,
                healed = self.healed,
                needs_reconcile = self.needs_reconcile,
                "Installed app runtime recovery restored every app it could find"
            );
        }
    }
}
