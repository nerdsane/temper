//! Ownership, layout and write-order cases of an app reinstall, including
//! store faults injected between the reads and the writes.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

/// What the wrapped store does once armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// Every write to `primary` errors.
    PrimaryWriteErrors,
    /// Another writer (a `PUT`) replaces `primary` just before ours lands.
    PrimaryReplacedFirst,
    /// Writing any app-owned row errors.
    AppRowWriteErrors,
}

/// Concurrent `PUT` text that must win over a reinstall's older snapshot.
const NEWER_PRIMARY: &str = "permit(principal, action == Action::\"Poke\", resource is Gadget);\npermit(principal, action == Action::\"Prod\", resource is Gadget);";

/// Forwards to the real store; injects `fault` once `armed` is set.
struct FaultyPolicyStore {
    inner: Arc<dyn PolicyStore>,
    fault: Fault,
    armed: Arc<AtomicBool>,
}

impl FaultyPolicyStore {
    fn wrap(
        fault: Fault,
        armed: &Arc<AtomicBool>,
    ) -> impl FnOnce(Arc<dyn PolicyStore>) -> Arc<dyn PolicyStore> {
        let armed = Arc::clone(armed);
        move |inner| {
            Arc::new(Self {
                inner,
                fault,
                armed,
            })
        }
    }

    fn active(&self, fault: Fault) -> bool {
        self.fault == fault && self.armed.load(Ordering::SeqCst)
    }

    /// Runs before any write to `primary`.
    async fn before_primary_write(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        if policy_id != "primary" {
            return Ok(());
        }
        if self.active(Fault::PrimaryWriteErrors) {
            return Err("injected failure writing primary".to_string());
        }
        if self.active(Fault::PrimaryReplacedFirst) {
            self.inner
                .save_policy(tenant, "primary", NEWER_PRIMARY, "concurrent-put")
                .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PolicyStore for FaultyPolicyStore {
    async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        if self.active(Fault::AppRowWriteErrors) && created_by.starts_with("os-app:") {
            return Err(format!("injected failure writing app row '{policy_id}'"));
        }
        self.inner
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        self.inner.load_policies_for_tenant(tenant).await
    }

    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        self.inner.load_all_policies().await
    }

    async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        self.inner
            .toggle_policy_enabled(tenant, policy_id, enabled)
            .await
    }

    async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.before_primary_write(tenant, policy_id).await?;
        self.inner
            .update_policy_text(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        policy_id: &str,
        expected_hash: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        self.before_primary_write(tenant, policy_id).await?;
        self.inner
            .replace_policy_if_hash(tenant, policy_id, expected_hash, cedar_text, created_by)
            .await
    }

    async fn replace_tenant_policies(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<(), String> {
        self.inner
            .replace_tenant_policies(tenant, policy_id, cedar_text, created_by)
            .await
    }

    async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        self.inner.delete_policy(tenant, policy_id).await
    }
}

/// Everything a failed install must leave as it was.
#[derive(Debug, PartialEq)]
struct PolicyState {
    rows: Vec<(String, String)>,
    legacy: String,
    live: String,
}

impl Fixture {
    async fn policy_state(&self) -> PolicyState {
        PolicyState {
            rows: self
                .rows()
                .await
                .into_iter()
                .map(|row| (row.policy_id, row.cedar_text))
                .collect(),
            legacy: self.legacy_text().await,
            live: self.live_text(),
        }
    }

    /// The statements a restart would load right now.
    async fn restart_statements(
        &self,
    ) -> std::collections::BTreeSet<temper_authz::statements::StatementKey> {
        let restarted = self.restart().await;
        let text = restarted
            .server
            .authz
            .get_tenant_policy_text(&self.tenant)
            .unwrap_or_default();
        temper_authz::statements::statement_keys(&text).expect("restart text parses")
    }
}

/// Install v1 (with the Withdraw forbid), then snapshot the live text plus a
/// hand-added permit as `primary`, as a `PUT` would.
async fn installed_with_primary(fx: &Fixture, app: &str) -> String {
    fx.write_app(app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(app).await;
    let snapshot = format!("{}\n{HAND_ADDED_POKE}", fx.live_text());
    fx.add_active_row("primary", &snapshot, &snapshot).await;
    fx.write_app(app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    snapshot
}

fn unique_app(label: &str) -> String {
    format!("reinstall-{label}-{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn another_owners_multi_statement_row_keeps_its_forbid() {
    let fx = Fixture::new("shared-statement").await;
    let app = format!("reinstall-shared-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    // A hand-added row that carries the app's old file, including its forbid,
    // among other statements of its own.
    let guard_row = format!(
        "{APPROVED_PING}\n{}",
        thing_policies(FORBID_WITHDRAW).trim()
    );
    let live = format!("{}\n{guard_row}", fx.live_text());
    fx.add_active_row("manual-withdraw-guard", &guard_row, &live)
        .await;

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(
            !fx.allowed(state, "Withdraw", "Thing"),
            "another owner's forbid was dropped"
        );
        assert!(fx.allowed(state, "Ping", "Gadget"));
        assert!(!fx.allowed(state, "Delete", "Thing"));
    }
}

#[tokio::test]
async fn reformatted_primary_copy_is_removed() {
    let fx = Fixture::new("reformatted-primary").await;
    let app = format!("reinstall-reformatted-{}", uuid::Uuid::new_v4().simple());
    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_WITHDRAW))]);
    fx.install(&app).await;
    let snapshot =
        format!("{PERMIT_THINGS}\r\n{FORBID_WITHDRAW_REFORMATTED}\r\n{HAND_ADDED_POKE}\r\n");
    fx.add_active_row("primary", &snapshot, &snapshot).await;
    assert!(!fx.allowed(&fx.state, "Withdraw", "Thing"));

    fx.write_app(&app, &[("thing.cedar", &thing_policies(FORBID_DELETE))]);
    fx.install(&app).await;

    for state in [&fx.state, &fx.restart().await] {
        assert!(
            fx.allowed(state, "Withdraw", "Thing"),
            "reformatted old forbid survived"
        );
        assert!(!fx.allowed(state, "Delete", "Thing"));
        assert!(
            fx.allowed(state, "Poke", "Gadget"),
            "hand-added policy lost"
        );
    }
    let rows = fx.rows().await;
    let primary = rows
        .iter()
        .find(|row| row.policy_id == "primary")
        .expect("primary policy row");
    assert!(
        primary.cedar_text.contains("// frozen items"),
        "layout around it kept"
    );
    assert!(primary.cedar_text.contains(HAND_ADDED_POKE));
}

#[tokio::test]
async fn failed_primary_rewrite_leaves_the_previous_install() {
    let armed = Arc::new(AtomicBool::new(false));
    let fx = Fixture::with_policy_store(
        "primary-fails",
        FaultyPolicyStore::wrap(Fault::PrimaryWriteErrors, &armed),
    )
    .await;
    let app = unique_app("primary-fails");
    let snapshot = installed_with_primary(&fx, &app).await;
    let before = fx.policy_state().await;

    armed.store(true, Ordering::SeqCst);
    let error = fx
        .try_install(&app)
        .await
        .expect_err("primary rewrite must fail");
    assert!(error.contains("primary"), "{error}");

    assert_eq!(
        fx.policy_state().await,
        before,
        "a failed install changed state"
    );
    assert_eq!(fx.live_text(), snapshot);
    for state in [&fx.state, &fx.restart().await] {
        assert!(
            !fx.allowed(state, "Withdraw", "Thing"),
            "previous install not intact"
        );
        assert!(fx.allowed(state, "Delete", "Thing"));
    }
}

#[tokio::test]
async fn concurrent_primary_put_wins_and_the_install_conflicts() {
    let armed = Arc::new(AtomicBool::new(false));
    let fx = Fixture::with_policy_store(
        "primary-put",
        FaultyPolicyStore::wrap(Fault::PrimaryReplacedFirst, &armed),
    )
    .await;
    let app = unique_app("primary-put");
    installed_with_primary(&fx, &app).await;
    let before = fx.policy_state().await;

    armed.store(true, Ordering::SeqCst);
    let error = fx
        .try_install(&app)
        .await
        .expect_err("a changed primary must stop the install");
    assert!(error.contains("Conflict"), "{error}");

    let after = fx.policy_state().await;
    let primary = after
        .rows
        .iter()
        .find(|(policy_id, _)| policy_id == "primary")
        .map(|(_, text)| text.as_str());
    assert_eq!(
        primary,
        Some(NEWER_PRIMARY),
        "the concurrent PUT was overwritten"
    );
    let others = |state: &PolicyState| -> Vec<(String, String)> {
        state
            .rows
            .iter()
            .filter(|(policy_id, _)| policy_id != "primary")
            .cloned()
            .collect()
    };
    assert_eq!(others(&after), others(&before), "app rows changed");
    assert_eq!(after.legacy, before.legacy, "aggregate changed");
    assert_eq!(after.live, before.live, "live policies changed");
}

#[tokio::test]
async fn failed_app_row_write_after_primary_rewrite_restarts_to_the_same_statements() {
    let armed = Arc::new(AtomicBool::new(false));
    let fx = Fixture::with_policy_store(
        "app-row-fails",
        FaultyPolicyStore::wrap(Fault::AppRowWriteErrors, &armed),
    )
    .await;
    let app = unique_app("app-row-fails");
    installed_with_primary(&fx, &app).await;
    let restart_before = fx.restart_statements().await;

    armed.store(true, Ordering::SeqCst);
    let error = fx
        .try_install(&app)
        .await
        .expect_err("the app row write must fail");
    assert!(error.contains("app row"), "{error}");

    let primary = fx
        .rows()
        .await
        .into_iter()
        .find(|row| row.policy_id == "primary")
        .expect("primary policy row");
    assert!(
        !primary.cedar_text.contains(FORBID_WITHDRAW),
        "the primary rewrite ran before the failure"
    );
    assert_eq!(
        fx.restart_statements().await,
        restart_before,
        "a restart after the failed install loads different policies"
    );
}
