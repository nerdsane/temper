//! Invocation-bound typed schema-deployment adapter over the shared service.

use temper_wasm_sdk::data::DataOperationKind;
use temper_wasm_sdk::schema_deployment::{
    SCHEMA_DEPLOYMENT_ABI_V1, SchemaDeploymentErrorV1, SchemaDeploymentOperationV1,
    SchemaDeploymentRequestV1, SchemaDeploymentResponseV1,
};

use super::ApplicationDataInvocation;
use crate::schema_deployment::GovernedSchemaDeploymentService;

impl ApplicationDataInvocation {
    pub(super) async fn call_schema_deployment_encoded(
        &self,
        request_bytes: usize,
        request: SchemaDeploymentRequestV1,
    ) -> Result<Vec<u8>, String> {
        let response = match self.admit_schema_deployment_call(request_bytes, &request) {
            Ok(kind) => {
                self.execute_schema_deployment(kind, request.operation)
                    .await
            }
            Err(error) => SchemaDeploymentResponseV1::Error { error },
        };
        let encoded = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
        if encoded.len() > self.authority.binding.grant.budgets.max_response_bytes as usize {
            return serde_json::to_vec(&SchemaDeploymentResponseV1::Error {
                error: schema_error(
                    "backend_unavailable",
                    "schema deployment response exceeded the invocation budget",
                    false,
                ),
            })
            .map_err(|error| error.to_string());
        }
        Ok(encoded)
    }

    fn admit_schema_deployment_call(
        &self,
        request_bytes: usize,
        request: &SchemaDeploymentRequestV1,
    ) -> Result<DataOperationKind, SchemaDeploymentErrorV1> {
        if request.abi != SCHEMA_DEPLOYMENT_ABI_V1 {
            return Err(schema_error(
                "invalid_bundle",
                "unsupported schema deployment ABI",
                false,
            ));
        }
        if request_bytes > self.authority.binding.grant.budgets.max_request_bytes as usize {
            return Err(schema_error(
                "invalid_bundle",
                "schema deployment request exceeded the invocation budget",
                false,
            ));
        }
        let kind = match request.operation {
            SchemaDeploymentOperationV1::Submit(_) => DataOperationKind::SchemaBundleSubmit,
            SchemaDeploymentOperationV1::GetBundle(_) => DataOperationKind::SchemaBundleGet,
            SchemaDeploymentOperationV1::Verify(_) => DataOperationKind::SchemaBundleVerify,
            SchemaDeploymentOperationV1::Activate(_) => DataOperationKind::SchemaBundleActivate,
            SchemaDeploymentOperationV1::Retire(_) => DataOperationKind::SchemaBundleRetire,
            SchemaDeploymentOperationV1::StartMigration(_) => {
                DataOperationKind::SchemaMigrationStart
            }
            SchemaDeploymentOperationV1::GetMigration(_) => DataOperationKind::SchemaMigrationGet,
            SchemaDeploymentOperationV1::RetryMigration(_) => {
                DataOperationKind::SchemaMigrationRetry
            }
            SchemaDeploymentOperationV1::StartStreamDescriptorMigration(_) => {
                DataOperationKind::StreamDescriptorMigrationStart
            }
            SchemaDeploymentOperationV1::AdvanceStreamDescriptorMigration(_) => {
                DataOperationKind::StreamDescriptorMigrationAdvance
            }
            SchemaDeploymentOperationV1::GetStreamDescriptorMigration(_) => {
                DataOperationKind::StreamDescriptorMigrationGet
            }
            SchemaDeploymentOperationV1::ListUnresolvedStreamDescriptors(_) => {
                DataOperationKind::StreamDescriptorMigrationListUnresolved
            }
        };
        if !self.authority.binding.grant.operations.contains(&kind) {
            return Err(schema_error(
                "authorization_denied",
                "module data grant does not permit this schema deployment operation",
                false,
            ));
        }
        let mut calls = self.calls.lock().map_err(|_| {
            schema_error(
                "backend_unavailable",
                "invocation call budget state is unavailable",
                true,
            )
        })?;
        *calls = calls.saturating_add(1);
        if *calls > self.authority.binding.grant.budgets.max_calls {
            return Err(schema_error(
                "backend_unavailable",
                "invocation call budget exhausted",
                false,
            ));
        }
        Ok(kind)
    }

    async fn execute_schema_deployment(
        &self,
        _kind: DataOperationKind,
        operation: SchemaDeploymentOperationV1,
    ) -> SchemaDeploymentResponseV1 {
        let service = GovernedSchemaDeploymentService::new(&self.state);
        let result = match operation {
            SchemaDeploymentOperationV1::Submit(request) => {
                service
                    .submit(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
            }
            SchemaDeploymentOperationV1::GetBundle(request) => {
                return match service
                    .get_request(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => SchemaDeploymentResponseV1::Ok { receipt },
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::Verify(request) => {
                service
                    .verify(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
            }
            SchemaDeploymentOperationV1::Activate(request) => {
                service
                    .activate(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
            }
            SchemaDeploymentOperationV1::Retire(request) => {
                service
                    .retire(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
            }
            SchemaDeploymentOperationV1::StartMigration(request) => {
                return match service
                    .start_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => SchemaDeploymentResponseV1::Migration { receipt },
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::GetMigration(request) => {
                return match service
                    .get_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => SchemaDeploymentResponseV1::Migration { receipt },
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::RetryMigration(request) => {
                return match service
                    .retry_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => SchemaDeploymentResponseV1::Migration { receipt },
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::StartStreamDescriptorMigration(request) => {
                return match service
                    .start_stream_descriptor_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => {
                        SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt }
                    }
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::AdvanceStreamDescriptorMigration(request) => {
                return match service
                    .advance_stream_descriptor_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => {
                        SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt }
                    }
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::GetStreamDescriptorMigration(request) => {
                return match service
                    .get_stream_descriptor_migration(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(receipt) => {
                        SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt }
                    }
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
            SchemaDeploymentOperationV1::ListUnresolvedStreamDescriptors(request) => {
                return match service
                    .list_unresolved_stream_descriptors(
                        self.authority.tenant.as_str(),
                        &self.authority.security,
                        request,
                    )
                    .await
                {
                    Ok(page) => SchemaDeploymentResponseV1::UnresolvedStreamDescriptors { page },
                    Err(error) => SchemaDeploymentResponseV1::Error {
                        error: error.into_contract(),
                    },
                };
            }
        };
        match result {
            Ok(receipt) => SchemaDeploymentResponseV1::Ok { receipt },
            Err(error) => SchemaDeploymentResponseV1::Error {
                error: error.into_contract(),
            },
        }
    }
}

fn schema_error(code: &str, message: &str, retryable: bool) -> SchemaDeploymentErrorV1 {
    SchemaDeploymentErrorV1 {
        code: code.into(),
        message: message.into(),
        retryable,
        decision_id: None,
    }
}
