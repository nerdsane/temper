//! Ordering and compensation for policy approval activation.

use std::future::Future;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ApprovalError {
    Persistence(String),
    Activation {
        activation: String,
        rollback: Option<String>,
    },
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Persistence(error) => write!(formatter, "approval persistence failed: {error}"),
            Self::Activation {
                activation,
                rollback: None,
            } => write!(
                formatter,
                "policy activation failed and durable approval was rolled back: {activation}"
            ),
            Self::Activation {
                activation,
                rollback: Some(rollback),
            } => write!(
                formatter,
                "policy activation failed ({activation}); durable rollback also failed ({rollback})"
            ),
        }
    }
}

/// Commit durable state first, activate only after commit, and compensate on
/// activation failure.
pub(super) async fn commit_then_activate<C, CFut, A, R, RFut>(
    commit: C,
    activate: A,
    rollback: R,
) -> Result<(), ApprovalError>
where
    C: FnOnce() -> CFut,
    CFut: Future<Output = Result<(), String>>,
    A: FnOnce() -> Result<(), String>,
    R: FnOnce() -> RFut,
    RFut: Future<Output = Result<(), String>>,
{
    commit().await.map_err(ApprovalError::Persistence)?;
    if let Err(activation) = activate() {
        let rollback = rollback().await.err();
        return Err(ApprovalError::Activation {
            activation,
            rollback,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{ApprovalError, commit_then_activate};

    #[tokio::test]
    async fn persistence_failure_never_activates_or_rolls_back() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = commit_then_activate(
            {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.lock().unwrap().push("commit");
                    Err("disk unavailable".to_string())
                }
            },
            {
                let calls = Arc::clone(&calls);
                move || {
                    calls.lock().unwrap().push("activate");
                    Ok(())
                }
            },
            {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.lock().unwrap().push("rollback");
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(
            result,
            Err(ApprovalError::Persistence("disk unavailable".to_string()))
        );
        assert_eq!(*calls.lock().unwrap(), vec!["commit"]);
    }

    #[tokio::test]
    async fn activation_failure_compensates_durable_state() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = commit_then_activate(
            {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.lock().unwrap().push("commit");
                    Ok(())
                }
            },
            {
                let calls = Arc::clone(&calls);
                move || {
                    calls.lock().unwrap().push("activate");
                    Err("engine unavailable".to_string())
                }
            },
            {
                let calls = Arc::clone(&calls);
                move || async move {
                    calls.lock().unwrap().push("rollback");
                    Ok(())
                }
            },
        )
        .await;

        assert_eq!(
            result,
            Err(ApprovalError::Activation {
                activation: "engine unavailable".to_string(),
                rollback: None,
            })
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["commit", "activate", "rollback"]
        );
    }

    #[tokio::test]
    async fn injected_activation_failure_restores_real_durable_rows() {
        let url = format!(
            "file:{}/temper-policy-approval-{}.db",
            std::env::temp_dir().display(),
            uuid::Uuid::new_v4() // determinism-ok: unique filename in a real-Turso unit test
        );
        let store = temper_store_turso::TursoEventStore::new(&url, None)
            .await
            .unwrap();
        let pending = serde_json::json!({
            "id": "decision-1",
            "tenant": "tenant-a",
            "status": "pending"
        })
        .to_string();
        let approved = serde_json::json!({
            "id": "decision-1",
            "tenant": "tenant-a",
            "status": "approved"
        })
        .to_string();
        store
            .upsert_pending_decision("decision-1", "tenant-a", "pending", &pending)
            .await
            .unwrap();

        let result = commit_then_activate(
            || async {
                store
                    .commit_policy_approval(temper_store_turso::TursoPolicyApprovalCommit {
                        tenant: "tenant-a",
                        decision_id: "decision-1",
                        approved_decision_json: &approved,
                        policy_id: "decision:decision-1",
                        cedar_text: "permit(principal, action, resource);",
                        created_by: "reviewer",
                    })
                    .await
                    .map_err(|error| error.to_string())
            },
            || Err("injected activation failure".to_string()),
            || async {
                store
                    .rollback_policy_approval(
                        "tenant-a",
                        "decision-1",
                        &pending,
                        "decision:decision-1",
                    )
                    .await
                    .map_err(|error| error.to_string())
            },
        )
        .await;

        assert!(matches!(result, Err(ApprovalError::Activation { .. })));
        assert!(
            store
                .load_policies_for_tenant("tenant-a")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .get_pending_decision("tenant-a", "decision-1")
                .await
                .unwrap()
                .unwrap(),
            pending
        );
    }
}
