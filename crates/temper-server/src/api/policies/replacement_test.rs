//! Deterministic storage schedules run the production replacement operation.
//! The simulated environment owns writes and faults; no network or clocks are used.
use super::*;
use crate::storage::PolicyStoreRow;
use std::sync::Mutex;

const OLD: &str = "forbid(principal, action == Action::\"old\", resource);";
const NEW: &str = "forbid(principal, action == Action::\"new\", resource);";
const OTHER: &str = "forbid(principal, action == Action::\"protected\", resource);";

struct SimStore {
    rows: Mutex<Vec<PolicyStoreRow>>,
    schedule: u8,
    reads: Mutex<usize>,
}
fn row(id: &str, text: &str) -> PolicyStoreRow {
    PolicyStoreRow {
        tenant: "demo".into(),
        policy_id: id.into(),
        cedar_text: text.into(),
        policy_hash: hash(text),
        created_at: String::new(),
        created_by: "fixture".into(),
        enabled: true,
    }
}
#[async_trait::async_trait]
impl PolicyStore for SimStore {
    async fn replace_policy_if_hash(
        &self,
        tenant: &str,
        id: &str,
        expected: &str,
        text: &str,
        by: &str,
    ) -> Result<bool, String> {
        let mut rows = self.rows.lock().unwrap();
        match self.schedule {
            1 => return Err("injected storage failure".into()),
            2 => rows[0] = row("legacy", OTHER), // Another human edited before commit.
            3 => rows[0].enabled = false,
            4 => {
                rows.remove(0);
            }
            _ => {}
        }
        let Some(row) = rows.iter_mut().find(|row| {
            row.tenant == tenant
                && row.policy_id == id
                && row.enabled
                && row.policy_hash == expected
        }) else {
            return Ok(false);
        };
        row.cedar_text = text.into();
        row.policy_hash = hash(text);
        row.created_by = by.into();
        if self.schedule == 5 {
            return Err("commit acknowledgment lost".into());
        }
        Ok(true)
    }
    async fn load_policies_for_tenant(&self, tenant: &str) -> Result<Vec<PolicyStoreRow>, String> {
        let mut reads = self.reads.lock().unwrap();
        *reads += 1;
        if self.schedule == 6 && *reads > 1 {
            return Err("readback unavailable".into());
        }
        let mut rows = self.rows.lock().unwrap();
        if self.schedule == 7 && *reads > 1 {
            rows[0] = row("legacy", OTHER);
        }
        Ok(rows
            .iter()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
    async fn save_policy(&self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, String> {
        panic!("unexpected unconditional save")
    }
    async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        panic!("unexpected cross-tenant read")
    }
    async fn toggle_policy_enabled(&self, _: &str, _: &str, _: bool) -> Result<bool, String> {
        panic!("unexpected enable mutation")
    }
    async fn update_policy_text(&self, _: &str, _: &str, _: &str, _: &str) -> Result<bool, String> {
        panic!("unexpected unconditional update")
    }
    async fn delete_policy(&self, _: &str, _: &str) -> Result<(), String> {
        panic!("unexpected delete")
    }
}

#[tokio::test]
async fn conditional_replacement_fault_schedules_preserve_invariants() {
    // Exhaustive environment schedules, stronger than randomly sampling these eight cases.
    for schedule in 0..8 {
        let engine = AuthzEngine::empty();
        engine
            .reload_tenant_policies_named(
                "demo",
                &[
                    ("legacy".into(), OLD.into()),
                    ("unrelated".into(), OTHER.into()),
                ],
            )
            .unwrap();
        let before = engine.get_tenant_policy_text("demo");
        let store = SimStore {
            rows: Mutex::new(vec![row("legacy", OLD), row("unrelated", OTHER)]),
            schedule,
            reads: Mutex::new(0),
        };
        let proposal = Replacement {
            expected_hash: hash(OLD),
            cedar_text: NEW.into(),
        };
        let result =
            replace_and_activate(&engine, &store, "demo", "legacy", &proposal, "human").await;
        assert_eq!(
            result.is_ok(),
            schedule == 0,
            "schedule={schedule}, result={result:?}"
        );
        let rows = store.rows.lock().unwrap();
        assert_eq!(
            rows.iter()
                .find(|row| row.policy_id == "unrelated")
                .unwrap()
                .cedar_text,
            OTHER
        );
        if schedule == 0 {
            assert_eq!(rows[0].cedar_text, NEW);
            assert_eq!(
                engine.get_tenant_policy_text("demo").unwrap(),
                format!("{NEW}\n{OTHER}")
            );
        } else {
            assert_eq!(
                engine.get_tenant_policy_text("demo"),
                before,
                "unverified proposal activated, schedule={schedule}"
            );
        }
    }
}

#[tokio::test]
async fn invalid_cedar_never_reaches_storage_or_active_engine() {
    let engine = AuthzEngine::empty();
    let store = SimStore {
        rows: Mutex::new(vec![row("legacy", OLD)]),
        schedule: 0,
        reads: Mutex::new(0),
    };
    for text in [
        "not cedar",
        "",
        "// only a comment",
        "permit(principal == ?principal, action, resource);",
    ] {
        let proposal = Replacement {
            expected_hash: hash(OLD),
            cedar_text: text.into(),
        };
        let error = replace_and_activate(&engine, &store, "demo", "legacy", &proposal, "human")
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(store.rows.lock().unwrap()[0].cedar_text, OLD);
        assert!(engine.get_tenant_policy_text("demo").is_none());
    }
}
