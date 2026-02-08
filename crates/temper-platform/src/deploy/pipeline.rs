//! The verify-and-deploy pipeline.
//!
//! Takes generated entity models, produces specs, runs the verification
//! cascade, and registers the tenant with hot-deployed entity actors.
//!
//! Emits OTEL spans for the full pipeline and per-entity verification:
//! ```text
//! temper.deploy (tenant, entity_count)
//!   └─ temper.verify.{Entity} (cascade_passed, l1, l2, l3)
//! ```

use opentelemetry::global;
use opentelemetry::trace::{Span, Status, Tracer};
use opentelemetry::KeyValue;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::{CascadeResult, VerificationCascade};

use crate::interview::{EntityModel, generate_ioa_toml, generate_csdl_xml};
use crate::protocol::{WsMessage, VerifyStepStatus};
use crate::state::PlatformState;

/// Result of a verify-and-deploy operation.
#[derive(Debug, Clone)]
pub struct DeployResult {
    /// Whether the entire pipeline succeeded.
    pub success: bool,
    /// Tenant that was deployed.
    pub tenant: String,
    /// Per-entity verification results.
    pub entity_results: Vec<EntityDeployResult>,
    /// Human-readable summary.
    pub summary: String,
}

/// Result for a single entity within the pipeline.
#[derive(Debug, Clone)]
pub struct EntityDeployResult {
    /// Entity type name.
    pub entity_name: String,
    /// Whether verification passed.
    pub verified: bool,
    /// The generated IOA TOML source.
    pub ioa_source: String,
    /// Cascade result details.
    pub cascade: Option<CascadeResult>,
}

/// Orchestrates the verify-and-deploy pipeline.
pub struct DeployPipeline;

impl DeployPipeline {
    /// Run the full verify-and-deploy pipeline.
    ///
    /// 1. Generate IOA TOML for each entity
    /// 2. Parse and validate each spec
    /// 3. Run verification cascade (L1/L2/L3)
    /// 4. Generate CSDL XML
    /// 5. Register tenant in the live SpecRegistry
    /// 6. Broadcast deployment status
    ///
    /// Emits a parent `temper.deploy` span with child spans per entity.
    pub fn verify_and_deploy(
        state: &PlatformState,
        tenant_name: &str,
        entities: &[EntityModel],
        namespace: &str,
    ) -> DeployResult {
        let tracer = global::tracer("temper");
        let mut deploy_span = tracer
            .span_builder("temper.deploy")
            .with_attributes(vec![
                KeyValue::new("temper.tenant", tenant_name.to_string()),
                KeyValue::new("temper.entity_count", entities.len() as i64),
            ])
            .start(&tracer);

        let mut entity_results = Vec::new();
        let mut all_passed = true;

        // Step 1-3: Generate, parse, and verify each entity
        for entity in entities {
            let mut entity_span = tracer
                .span_builder(format!("temper.verify.{}", entity.name))
                .with_attributes(vec![
                    KeyValue::new("temper.entity", entity.name.clone()),
                    KeyValue::new("temper.tenant", tenant_name.to_string()),
                ])
                .start(&tracer);

            state.broadcast(WsMessage::VerifyStatus {
                level: format!("Verifying {}", entity.name),
                status: VerifyStepStatus::Running,
                summary: format!("Generating and verifying spec for {}", entity.name),
            });

            let ioa_source = generate_ioa_toml(entity);

            // Parse the generated spec to validate it
            let parse_result = automaton::parse_automaton(&ioa_source);
            if let Err(e) = &parse_result {
                state.broadcast(WsMessage::VerifyStatus {
                    level: format!("{} Parse", entity.name),
                    status: VerifyStepStatus::Failed,
                    summary: format!("Failed to parse IOA spec: {e}"),
                });
                entity_span.set_status(Status::Error {
                    description: format!("parse failed: {e}").into(),
                });
                entity_span.set_attribute(KeyValue::new("temper.cascade_passed", false));
                entity_span.end();
                entity_results.push(EntityDeployResult {
                    entity_name: entity.name.clone(),
                    verified: false,
                    ioa_source,
                    cascade: None,
                });
                all_passed = false;
                continue;
            }

            // Run verification cascade
            state.broadcast(WsMessage::VerifyStatus {
                level: "L1 Model Check".into(),
                status: VerifyStepStatus::Running,
                summary: format!("Running model check for {}", entity.name),
            });

            let cascade = VerificationCascade::from_ioa(&ioa_source)
                .with_sim_seeds(5)
                .with_prop_test_cases(100);
            let result = cascade.run();

            // Broadcast per-level results
            for level_result in &result.levels {
                let status = if level_result.passed {
                    VerifyStepStatus::Passed
                } else {
                    VerifyStepStatus::Failed
                };
                state.broadcast(WsMessage::VerifyStatus {
                    level: format!("{}", level_result.level),
                    status,
                    summary: level_result.summary.clone(),
                });
            }

            // Record per-level results on the entity span
            for (i, level_result) in result.levels.iter().enumerate() {
                let level_key = format!("temper.l{}", i + 1);
                let val = if level_result.passed { "PASS" } else { "FAIL" };
                entity_span.set_attribute(KeyValue::new(level_key, val));
            }

            let verified = result.all_passed;
            entity_span.set_attribute(KeyValue::new("temper.cascade_passed", verified));
            if !verified {
                all_passed = false;
                entity_span.set_status(Status::Error {
                    description: "verification failed".into(),
                });
            }
            entity_span.end();

            entity_results.push(EntityDeployResult {
                entity_name: entity.name.clone(),
                verified,
                ioa_source,
                cascade: Some(result),
            });
        }

        // Step 4-5: If all verified, generate CSDL and register tenant
        if all_passed && !entities.is_empty() {
            let csdl_xml = generate_csdl_xml(entities, namespace);

            // Parse CSDL to validate
            match parse_csdl(&csdl_xml) {
                Ok(csdl) => {
                    // Collect IOA sources for registration
                    let ioa_pairs: Vec<(&str, &str)> = entity_results
                        .iter()
                        .map(|r| (r.entity_name.as_str(), r.ioa_source.as_str()))
                        .collect();

                    // Register tenant in the live registry
                    {
                        let mut registry = state.registry.write().unwrap();
                        registry.register_tenant(
                            TenantId::new(tenant_name),
                            csdl,
                            csdl_xml,
                            &ioa_pairs,
                        );
                    }

                    state.broadcast(WsMessage::DeployStatus {
                        tenant: tenant_name.to_string(),
                        success: true,
                        summary: format!(
                            "Deployed {} entities for tenant '{}'",
                            entities.len(),
                            tenant_name,
                        ),
                    });
                }
                Err(e) => {
                    all_passed = false;
                    deploy_span.set_status(Status::Error {
                        description: format!("CSDL failed: {e}").into(),
                    });
                    state.broadcast(WsMessage::DeployStatus {
                        tenant: tenant_name.to_string(),
                        success: false,
                        summary: format!("CSDL generation failed: {e}"),
                    });
                }
            }
        } else if !all_passed {
            deploy_span.set_status(Status::Error {
                description: "verification failed".into(),
            });
            state.broadcast(WsMessage::DeployStatus {
                tenant: tenant_name.to_string(),
                success: false,
                summary: "Deployment aborted: verification failed".into(),
            });
        }

        deploy_span.set_attribute(KeyValue::new("temper.success", all_passed));
        deploy_span.end();

        let summary = if all_passed {
            format!(
                "Successfully deployed {} entities for tenant '{tenant_name}'",
                entities.len(),
            )
        } else {
            let failed: Vec<&str> = entity_results
                .iter()
                .filter(|r| !r.verified)
                .map(|r| r.entity_name.as_str())
                .collect();
            format!("Deployment failed: verification failed for {:?}", failed)
        };

        DeployResult {
            success: all_passed,
            tenant: tenant_name.to_string(),
            entity_results,
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview::{StateDefinition, ActionDefinition, ActionKind, StateVariable, InvariantDefinition};

    fn sample_task_entity() -> EntityModel {
        EntityModel {
            name: "Task".into(),
            description: "A simple task tracker".into(),
            states: vec![
                StateDefinition {
                    name: "Open".into(),
                    description: "Task is open".into(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "InProgress".into(),
                    description: "Task is being worked on".into(),
                    is_terminal: false,
                },
                StateDefinition {
                    name: "Done".into(),
                    description: "Task is completed".into(),
                    is_terminal: true,
                },
            ],
            actions: vec![
                ActionDefinition {
                    name: "StartWork".into(),
                    from_states: vec!["Open".into()],
                    to_state: Some("InProgress".into()),
                    guard: None,
                    params: vec![],
                    hint: Some("Begin working on the task".into()),
                    kind: ActionKind::Internal,
                },
                ActionDefinition {
                    name: "Complete".into(),
                    from_states: vec!["InProgress".into()],
                    to_state: Some("Done".into()),
                    guard: None,
                    params: vec![],
                    hint: Some("Mark task as done".into()),
                    kind: ActionKind::Internal,
                },
            ],
            invariants: vec![],
            state_variables: vec![],
        }
    }

    #[test]
    fn test_deploy_pipeline_success() {
        let state = PlatformState::new_dev(None);
        let mut rx = state.subscribe();

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "test-tenant",
            &[sample_task_entity()],
            "Test.TaskTracker",
        );

        assert!(result.success, "Pipeline should succeed: {}", result.summary);
        assert_eq!(result.tenant, "test-tenant");
        assert_eq!(result.entity_results.len(), 1);
        assert!(result.entity_results[0].verified);

        // Verify broadcast messages were sent
        let mut received = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            received.push(msg);
        }
        assert!(!received.is_empty(), "Should have broadcast messages");
    }

    #[test]
    fn test_deploy_pipeline_registers_tenant() {
        let state = PlatformState::new_dev(None);

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "registered-tenant",
            &[sample_task_entity()],
            "Test.TaskTracker",
        );

        assert!(result.success);

        // Verify tenant was registered
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("registered-tenant");
        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Task").is_some());
    }

    #[test]
    fn test_deploy_pipeline_empty_entities() {
        let state = PlatformState::new_dev(None);

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "empty-tenant",
            &[],
            "Test.Empty",
        );

        // Empty entities should succeed vacuously
        assert!(result.success);
        assert!(result.entity_results.is_empty());
    }

    #[test]
    fn test_deploy_pipeline_verification_results() {
        let state = PlatformState::new_dev(None);

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "verify-tenant",
            &[sample_task_entity()],
            "Test.TaskTracker",
        );

        assert!(result.success);
        let entity_result = &result.entity_results[0];
        assert!(entity_result.cascade.is_some());
        let cascade = entity_result.cascade.as_ref().unwrap();
        assert!(cascade.all_passed);
    }

    #[test]
    fn test_deploy_pipeline_broadcasts_verify_status() {
        let state = PlatformState::new_dev(None);
        let mut rx = state.subscribe();

        let _result = DeployPipeline::verify_and_deploy(
            &state,
            "broadcast-tenant",
            &[sample_task_entity()],
            "Test.TaskTracker",
        );

        let mut verify_msgs = Vec::new();
        let mut deploy_msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match &msg {
                WsMessage::VerifyStatus { .. } => verify_msgs.push(msg),
                WsMessage::DeployStatus { .. } => deploy_msgs.push(msg),
                _ => {}
            }
        }

        assert!(!verify_msgs.is_empty(), "Should have verify status broadcasts");
        assert_eq!(deploy_msgs.len(), 1, "Should have exactly one deploy status");
    }

    #[test]
    fn test_deploy_result_summary() {
        let state = PlatformState::new_dev(None);

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "summary-tenant",
            &[sample_task_entity()],
            "Test.TaskTracker",
        );

        assert!(result.summary.contains("Successfully deployed"));
        assert!(result.summary.contains("summary-tenant"));
    }

    #[test]
    fn test_deploy_pipeline_span_noop() {
        // Verifies that OTEL span instrumentation in verify_and_deploy
        // doesn't panic when no OTEL provider is initialized (no-op tracer).
        let state = PlatformState::new_dev(None);
        let result = DeployPipeline::verify_and_deploy(
            &state,
            "noop-tenant",
            &[sample_task_entity()],
            "Test.Noop",
        );
        assert!(result.success, "Pipeline should succeed with no-op OTEL: {}", result.summary);
    }

    #[test]
    fn test_deploy_multiple_entities() {
        let state = PlatformState::new_dev(None);

        let mut bug_entity = sample_task_entity();
        bug_entity.name = "Bug".into();
        bug_entity.description = "A bug report".into();

        let result = DeployPipeline::verify_and_deploy(
            &state,
            "multi-tenant",
            &[sample_task_entity(), bug_entity],
            "Test.ProjectMgmt",
        );

        assert!(result.success, "Pipeline should succeed: {}", result.summary);
        assert_eq!(result.entity_results.len(), 2);

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("multi-tenant");
        assert!(registry.get_table(&tenant, "Task").is_some());
        assert!(registry.get_table(&tenant, "Bug").is_some());
    }
}
