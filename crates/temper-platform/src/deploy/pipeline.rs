//! The verify-and-deploy pipeline.
//!
//! Takes pre-authored specs (IOA TOML + CSDL XML), runs the verification
//! cascade, and registers the tenant with hot-deployed entity actors.
//!
//! Emits OTEL spans for the full pipeline and per-entity verification:
//! ```text
//! temper.deploy (tenant, entity_count)
//!   └─ temper.verify.{Entity} (cascade_passed, l1, l2, l3)
//! ```

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::{Span, Status, Tracer};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton;
use temper_spec::csdl::parse_csdl;
use temper_verify::cascade::{CascadeResult, VerificationCascade};

use crate::protocol::{PlatformEvent, VerifyStepStatus};
use crate::state::PlatformState;

/// A pre-authored entity spec source.
#[derive(Debug, Clone)]
pub struct EntitySpecSource {
    /// Entity type name (PascalCase, e.g. "Order").
    pub entity_type: String,
    /// Raw IOA TOML source.
    pub ioa_source: String,
}

/// Input for the verify-and-deploy pipeline.
#[derive(Debug, Clone)]
pub struct DeployInput {
    /// Tenant name to register.
    pub tenant_name: String,
    /// CSDL XML schema for this tenant's entities.
    pub csdl_xml: String,
    /// Pre-authored entity specs.
    pub entities: Vec<EntitySpecSource>,
    /// WASM modules for integration handlers: module_name → wasm_bytes.
    pub wasm_modules: std::collections::BTreeMap<String, Vec<u8>>,
}

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
    /// The IOA TOML source.
    pub ioa_source: String,
    /// Cascade result details.
    pub cascade: Option<CascadeResult>,
}

/// Orchestrates the verify-and-deploy pipeline.
pub struct DeployPipeline;

impl DeployPipeline {
    /// Run the full verify-and-deploy pipeline.
    ///
    /// 1. Parse and validate each IOA spec
    /// 2. Run verification cascade (L1/L2/L3) per entity
    /// 3. Parse CSDL XML
    /// 4. Register tenant in the live SpecRegistry
    /// 5. Broadcast deployment status
    ///
    /// Emits a parent `temper.deploy` span with child spans per entity.
    pub fn verify_and_deploy(state: &PlatformState, input: &DeployInput) -> DeployResult {
        let tracer = global::tracer("temper");
        let mut deploy_span = tracer
            .span_builder("temper.deploy")
            .with_attributes(vec![
                KeyValue::new("temper.tenant", input.tenant_name.clone()),
                KeyValue::new("temper.entity_count", input.entities.len() as i64),
            ])
            .start(&tracer);

        let mut entity_results = Vec::new();
        let mut all_passed = true;

        // Step 1-2: Parse and verify each entity spec
        for entity in &input.entities {
            let mut entity_span = tracer
                .span_builder(format!("temper.verify.{}", entity.entity_type))
                .with_attributes(vec![
                    KeyValue::new("temper.entity", entity.entity_type.clone()),
                    KeyValue::new("temper.tenant", input.tenant_name.clone()),
                ])
                .start(&tracer);

            state.broadcast(PlatformEvent::VerifyStatus {
                tenant: input.tenant_name.clone(),
                level: format!("Verifying {}", entity.entity_type),
                status: VerifyStepStatus::Running,
                summary: format!("Parsing and verifying spec for {}", entity.entity_type),
            });

            // Parse the IOA spec
            let parse_result = automaton::parse_automaton(&entity.ioa_source);
            if let Err(e) = &parse_result {
                state.broadcast(PlatformEvent::VerifyStatus {
                    tenant: input.tenant_name.clone(),
                    level: format!("{} Parse", entity.entity_type),
                    status: VerifyStepStatus::Failed,
                    summary: format!("Failed to parse IOA spec: {e}"),
                });
                entity_span.set_status(Status::Error {
                    description: format!("parse failed: {e}").into(),
                });
                entity_span.set_attribute(KeyValue::new("temper.cascade_passed", false));
                entity_span.end();
                entity_results.push(EntityDeployResult {
                    entity_name: entity.entity_type.clone(),
                    verified: false,
                    ioa_source: entity.ioa_source.clone(),
                    cascade: None,
                });
                all_passed = false;
                continue;
            }

            // Validate WASM integration modules: every type="wasm" integration
            // must reference a module present in `input.wasm_modules`.
            if let Ok(ref automaton) = parse_result {
                let mut wasm_ok = true;
                for integration in &automaton.integrations {
                    if integration.integration_type == "wasm"
                        && let Some(ref module_name) = integration.module
                        && !input.wasm_modules.contains_key(module_name)
                    {
                        state.broadcast(PlatformEvent::VerifyStatus {
                                    tenant: input.tenant_name.clone(),
                                    level: format!("{} WASM", entity.entity_type),
                                    status: VerifyStepStatus::Failed,
                                    summary: format!(
                                        "WASM module '{}' required by integration '{}' not found in deploy input",
                                        module_name, integration.name,
                                    ),
                                });
                        wasm_ok = false;
                    }
                }
                if !wasm_ok {
                    entity_span.set_status(Status::Error {
                        description: "missing WASM modules".into(),
                    });
                    entity_span.set_attribute(KeyValue::new("temper.cascade_passed", false));
                    entity_span.end();
                    entity_results.push(EntityDeployResult {
                        entity_name: entity.entity_type.clone(),
                        verified: false,
                        ioa_source: entity.ioa_source.clone(),
                        cascade: None,
                    });
                    all_passed = false;
                    continue;
                }
            }

            // Run verification cascade
            state.broadcast(PlatformEvent::VerifyStatus {
                tenant: input.tenant_name.clone(),
                level: "L1 Model Check".into(),
                status: VerifyStepStatus::Running,
                summary: format!("Running model check for {}", entity.entity_type),
            });

            let cascade = VerificationCascade::from_ioa(&entity.ioa_source)
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
                state.broadcast(PlatformEvent::VerifyStatus {
                    tenant: input.tenant_name.clone(),
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
                entity_name: entity.entity_type.clone(),
                verified,
                ioa_source: entity.ioa_source.clone(),
                cascade: Some(result),
            });
        }

        // Step 3-4: If all verified, parse CSDL and register tenant
        if all_passed && !input.entities.is_empty() {
            match parse_csdl(&input.csdl_xml) {
                Ok(csdl) => {
                    // Collect IOA sources for registration
                    let ioa_pairs: Vec<(&str, &str)> = entity_results
                        .iter()
                        .map(|r| (r.entity_name.as_str(), r.ioa_source.as_str()))
                        .collect();

                    // Register tenant in the live registry.
                    let register_result = {
                        let mut registry = state.registry.write().unwrap();
                        registry.try_register_tenant(
                            TenantId::new(&input.tenant_name),
                            csdl,
                            input.csdl_xml.clone(),
                            &ioa_pairs,
                        )
                    };

                    match register_result {
                        Ok(()) => {
                            state.broadcast(PlatformEvent::DeployStatus {
                                tenant: input.tenant_name.clone(),
                                success: true,
                                summary: format!(
                                    "Deployed {} entities for tenant '{}'",
                                    input.entities.len(),
                                    input.tenant_name,
                                ),
                            });

                            state.broadcast(PlatformEvent::TenantRegistered {
                                tenant: input.tenant_name.clone(),
                                entity_count: input.entities.len(),
                            });
                        }
                        Err(e) => {
                            all_passed = false;
                            deploy_span.set_status(Status::Error {
                                description: format!("registry registration failed: {e}").into(),
                            });
                            state.broadcast(PlatformEvent::DeployStatus {
                                tenant: input.tenant_name.clone(),
                                success: false,
                                summary: format!("Tenant registration failed: {e}"),
                            });
                        }
                    }
                }
                Err(e) => {
                    all_passed = false;
                    deploy_span.set_status(Status::Error {
                        description: format!("CSDL failed: {e}").into(),
                    });
                    state.broadcast(PlatformEvent::DeployStatus {
                        tenant: input.tenant_name.clone(),
                        success: false,
                        summary: format!("CSDL parsing failed: {e}"),
                    });
                }
            }
        } else if !all_passed {
            deploy_span.set_status(Status::Error {
                description: "verification failed".into(),
            });
            state.broadcast(PlatformEvent::DeployStatus {
                tenant: input.tenant_name.clone(),
                success: false,
                summary: "Deployment aborted: verification failed".into(),
            });
        }

        deploy_span.set_attribute(KeyValue::new("temper.success", all_passed));
        deploy_span.end();

        let summary = if all_passed {
            format!(
                "Successfully deployed {} entities for tenant '{}'",
                input.entities.len(),
                input.tenant_name,
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
            tenant: input.tenant_name.clone(),
            entity_results,
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_IOA: &str = r#"
[automaton]
name = "Task"
initial = "Open"
states = ["Open", "InProgress", "Done"]

[[action]]
name = "StartWork"
from = ["Open"]
to = "InProgress"
kind = "internal"

[[action]]
name = "Complete"
from = ["InProgress"]
to = "Done"
kind = "internal"
"#;

    const TASK_CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.TaskTracker" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Task">
        <Key>
          <PropertyRef Name="Id" />
        </Key>
        <Property Name="Id" Type="Edm.String" Nullable="false" />
        <Property Name="Status" Type="Edm.String" />
      </EntityType>
      <Action Name="StartWork" IsBound="true">
        <Parameter Name="bindingParameter" Type="Test.TaskTracker.Task" />
      </Action>
      <Action Name="Complete" IsBound="true">
        <Parameter Name="bindingParameter" Type="Test.TaskTracker.Task" />
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Tasks" EntityType="Test.TaskTracker.Task" />
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

    fn sample_deploy_input() -> DeployInput {
        DeployInput {
            tenant_name: "test-tenant".into(),
            csdl_xml: TASK_CSDL.into(),
            entities: vec![EntitySpecSource {
                entity_type: "Task".into(),
                ioa_source: TASK_IOA.into(),
            }],
            wasm_modules: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn test_deploy_pipeline_success() {
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        let result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());

        assert!(
            result.success,
            "Pipeline should succeed: {}",
            result.summary
        );
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
        let state = PlatformState::new(None);

        let result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());

        assert!(result.success);

        // Verify tenant was registered
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-tenant");
        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Task").is_some());
    }

    #[test]
    fn test_deploy_pipeline_empty_entities() {
        let state = PlatformState::new(None);

        let input = DeployInput {
            tenant_name: "empty-tenant".into(),
            csdl_xml: TASK_CSDL.into(),
            entities: vec![],
            wasm_modules: std::collections::BTreeMap::new(),
        };
        let result = DeployPipeline::verify_and_deploy(&state, &input);

        // Empty entities should succeed vacuously
        assert!(result.success);
        assert!(result.entity_results.is_empty());
    }

    #[test]
    fn test_deploy_pipeline_verification_results() {
        let state = PlatformState::new(None);

        let result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());

        assert!(result.success);
        let entity_result = &result.entity_results[0];
        assert!(entity_result.cascade.is_some());
        let cascade = entity_result.cascade.as_ref().unwrap();
        assert!(cascade.all_passed);
    }

    #[test]
    fn test_deploy_pipeline_broadcasts_verify_status() {
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        let _result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());

        let mut verify_msgs = Vec::new();
        let mut deploy_msgs = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            match &msg {
                PlatformEvent::VerifyStatus { .. } => verify_msgs.push(msg),
                PlatformEvent::DeployStatus { .. } => deploy_msgs.push(msg),
                _ => {}
            }
        }

        assert!(
            !verify_msgs.is_empty(),
            "Should have verify status broadcasts"
        );
        assert_eq!(
            deploy_msgs.len(),
            1,
            "Should have exactly one deploy status"
        );
    }

    #[test]
    fn test_deploy_result_summary() {
        let state = PlatformState::new(None);

        let result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());

        assert!(result.summary.contains("Successfully deployed"));
        assert!(result.summary.contains("test-tenant"));
    }

    #[test]
    fn test_deploy_pipeline_span_noop() {
        // Verifies that OTEL span instrumentation doesn't panic with no-op tracer.
        let state = PlatformState::new(None);
        let result = DeployPipeline::verify_and_deploy(&state, &sample_deploy_input());
        assert!(
            result.success,
            "Pipeline should succeed with no-op OTEL: {}",
            result.summary
        );
    }

    #[test]
    fn test_deploy_multiple_entities() {
        let state = PlatformState::new(None);

        let input = DeployInput {
            tenant_name: "multi-tenant".into(),
            csdl_xml: TASK_CSDL.into(),
            entities: vec![
                EntitySpecSource {
                    entity_type: "Task".into(),
                    ioa_source: TASK_IOA.into(),
                },
                EntitySpecSource {
                    entity_type: "Bug".into(),
                    ioa_source: TASK_IOA.replace("Task", "Bug"),
                },
            ],
            wasm_modules: std::collections::BTreeMap::new(),
        };

        let result = DeployPipeline::verify_and_deploy(&state, &input);

        assert!(
            result.success,
            "Pipeline should succeed: {}",
            result.summary
        );
        assert_eq!(result.entity_results.len(), 2);

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("multi-tenant");
        assert!(registry.get_table(&tenant, "Task").is_some());
        assert!(registry.get_table(&tenant, "Bug").is_some());
    }

    #[test]
    fn test_deploy_bad_ioa_fails() {
        let state = PlatformState::new(None);

        let input = DeployInput {
            tenant_name: "bad-tenant".into(),
            csdl_xml: TASK_CSDL.into(),
            entities: vec![EntitySpecSource {
                entity_type: "Bad".into(),
                ioa_source: "this is not valid TOML".into(),
            }],
            wasm_modules: std::collections::BTreeMap::new(),
        };

        let result = DeployPipeline::verify_and_deploy(&state, &input);
        assert!(!result.success);
        assert!(!result.entity_results[0].verified);
    }
}
