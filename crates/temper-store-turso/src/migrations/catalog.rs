use sha2::{Digest, Sha256};

use super::ots_rebuild::OTS_REBUILD_DEFINITION;
use crate::schema;

pub(super) const VALIDATION_MANIFEST_VERSION: &str = "length-prefixed-schema-snapshot-v10";

#[derive(Clone, Copy, Debug)]
pub(super) enum MigrationStep {
    Sql(&'static str),
    AddColumn {
        table: &'static str,
        column: &'static str,
        sql: &'static str,
    },
    RebuildOtsTrajectories,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub steps: &'static [MigrationStep],
}

impl Migration {
    pub fn checksum(&self, schema_manifest: &str) -> String {
        let mut hasher = Sha256::new();
        hash_part(&mut hasher, b"temper-turso-migration-v1");
        hash_part(&mut hasher, &self.version.to_be_bytes());
        hash_part(&mut hasher, self.name.as_bytes());
        hash_part(&mut hasher, VALIDATION_MANIFEST_VERSION.as_bytes());
        for step in self.steps {
            match step {
                MigrationStep::Sql(sql) => {
                    hash_part(&mut hasher, b"sql");
                    hash_part(&mut hasher, sql.as_bytes());
                }
                MigrationStep::AddColumn { table, column, sql } => {
                    hash_part(&mut hasher, b"add-column");
                    hash_part(&mut hasher, table.as_bytes());
                    hash_part(&mut hasher, column.as_bytes());
                    hash_part(&mut hasher, sql.as_bytes());
                }
                MigrationStep::RebuildOtsTrajectories => {
                    let definition = &OTS_REBUILD_DEFINITION;
                    hash_part(&mut hasher, b"rebuild-ots-trajectories");
                    hash_part(&mut hasher, definition.algorithm_version.as_bytes());
                    hash_part(&mut hasher, definition.table.as_bytes());
                    hash_part(&mut hasher, definition.temporary_table.as_bytes());
                    for column in definition.required_columns {
                        hash_part(&mut hasher, column.name.as_bytes());
                        hash_part(&mut hasher, column.affinity.as_bytes());
                        hash_part(&mut hasher, &[u8::from(column.not_null)]);
                        hash_part(&mut hasher, column.default.unwrap_or("<none>").as_bytes());
                        hash_part(&mut hasher, &column.primary_key_position.to_be_bytes());
                    }
                    let column = definition.updated_at_column;
                    hash_part(&mut hasher, column.name.as_bytes());
                    hash_part(&mut hasher, column.affinity.as_bytes());
                    hash_part(&mut hasher, &[u8::from(column.not_null)]);
                    hash_part(&mut hasher, column.default.unwrap_or("<none>").as_bytes());
                    hash_part(&mut hasher, &column.primary_key_position.to_be_bytes());
                    for sequence in definition.forbidden_table_sql_sequences {
                        hash_part(&mut hasher, &(sequence.len() as u64).to_be_bytes());
                        for token in *sequence {
                            hash_part(&mut hasher, token.as_bytes());
                        }
                    }
                    hash_part(&mut hasher, definition.schema_tables_query.as_bytes());
                    hash_part(&mut hasher, definition.dependent_objects_query.as_bytes());
                    hash_part(&mut hasher, definition.create_temporary_sql.as_bytes());
                    hash_part(&mut hasher, definition.copy_sql.as_bytes());
                    hash_part(&mut hasher, definition.drop_sql.as_bytes());
                    hash_part(&mut hasher, definition.rename_sql.as_bytes());
                }
            }
        }
        hash_part(&mut hasher, schema_manifest.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

const JOURNAL_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_EVENTS_TABLE),
    MigrationStep::AddColumn {
        table: "events",
        column: "segment_index",
        sql: schema::ALTER_EVENTS_ADD_SEGMENT_INDEX,
    },
    MigrationStep::Sql(schema::CREATE_EVENTS_ENTITY_INDEX),
    MigrationStep::Sql(schema::CREATE_EVENT_SEGMENTS_TABLE),
    MigrationStep::Sql(schema::CREATE_EVENT_SEGMENTS_OPEN_INDEX),
    MigrationStep::Sql(schema::CREATE_SNAPSHOTS_TABLE),
    MigrationStep::Sql(schema::CREATE_SNAPSHOT_HISTORY_TABLE),
    MigrationStep::Sql(schema::CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX),
];

const SPEC_INTEGRATION_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_SPECS_TABLE),
    MigrationStep::AddColumn {
        table: "specs",
        column: "content_hash",
        sql: schema::ALTER_SPECS_ADD_CONTENT_HASH,
    },
    MigrationStep::AddColumn {
        table: "specs",
        column: "committed",
        sql: schema::ALTER_SPECS_ADD_COMMITTED,
    },
    MigrationStep::Sql(schema::CREATE_TENANT_CONSTRAINTS_TABLE),
    MigrationStep::Sql(schema::CREATE_WASM_MODULES_TABLE),
    MigrationStep::AddColumn {
        table: "wasm_modules",
        column: "source",
        sql: schema::ADD_WASM_MODULES_SOURCE_COLUMN,
    },
    MigrationStep::Sql(schema::CREATE_WASM_INVOCATION_LOGS_TABLE),
    MigrationStep::Sql(schema::CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX),
    MigrationStep::Sql(schema::CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX),
    MigrationStep::Sql(schema::CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX),
];

const AUTHORIZATION_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_PENDING_DECISIONS_TABLE),
    MigrationStep::Sql(schema::CREATE_PENDING_DECISIONS_TENANT_INDEX),
    MigrationStep::Sql(schema::CREATE_PENDING_DECISIONS_STATUS_INDEX),
    MigrationStep::Sql(schema::CREATE_TENANT_POLICIES_TABLE),
    MigrationStep::Sql(schema::CREATE_POLICIES_TABLE),
    MigrationStep::AddColumn {
        table: "policies",
        column: "enabled",
        sql: schema::ALTER_POLICIES_ADD_ENABLED,
    },
    MigrationStep::Sql(schema::CREATE_POLICY_DENIAL_PATTERNS_TABLE),
    MigrationStep::Sql(schema::CREATE_POLICY_DENIAL_PATTERNS_TENANT_INDEX),
    MigrationStep::Sql(schema::CREATE_PUBLISHED_ARTIFACTS_TABLE),
    MigrationStep::Sql(schema::CREATE_PUBLISHED_ARTIFACTS_OWNER_INDEX),
    MigrationStep::Sql(schema::CREATE_PUBLISHED_ARTIFACTS_SOURCE_INDEX),
];

const APP_PLATFORM_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_TENANT_INSTALLED_APPS_TABLE),
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "app_version",
        sql: schema::ALTER_INSTALLED_APPS_ADD_APP_VERSION,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "source_kind",
        sql: schema::ALTER_INSTALLED_APPS_ADD_SOURCE_KIND,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "app_ref",
        sql: schema::ALTER_INSTALLED_APPS_ADD_APP_REF,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "version_hash",
        sql: schema::ALTER_INSTALLED_APPS_ADD_VERSION_HASH,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "pinned_version_hash",
        sql: schema::ALTER_INSTALLED_APPS_ADD_PINNED_VERSION_HASH,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "current_version_hash",
        sql: schema::ALTER_INSTALLED_APPS_ADD_CURRENT_VERSION_HASH,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "follow_policy",
        sql: schema::ALTER_INSTALLED_APPS_ADD_FOLLOW_POLICY,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "closure_id",
        sql: schema::ALTER_INSTALLED_APPS_ADD_CLOSURE_ID,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "registry_url",
        sql: schema::ALTER_INSTALLED_APPS_ADD_REGISTRY_URL,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "registry_tenant",
        sql: schema::ALTER_INSTALLED_APPS_ADD_REGISTRY_TENANT,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "bundle_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_BUNDLE_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "spec_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_SPEC_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "policy_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_POLICY_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "wasm_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_WASM_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "content_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_CONTENT_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "seed_digest",
        sql: schema::ALTER_INSTALLED_APPS_ADD_SEED_DIGEST,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "last_reconciled_at",
        sql: schema::ALTER_INSTALLED_APPS_ADD_LAST_RECONCILED_AT,
    },
    MigrationStep::AddColumn {
        table: "tenant_installed_apps",
        column: "status",
        sql: schema::ALTER_INSTALLED_APPS_ADD_STATUS,
    },
    MigrationStep::Sql(schema::CREATE_TENANT_REGISTRY_TABLE),
    MigrationStep::Sql(schema::CREATE_TENANT_USERS_TABLE),
    MigrationStep::Sql(schema::CREATE_TENANT_USERS_USER_INDEX),
    MigrationStep::Sql(schema::CREATE_TENANT_SECRETS_TABLE),
];

const TRAJECTORY_EVOLUTION_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_TRAJECTORIES_TABLE),
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "agent_id",
        sql: schema::ALTER_TRAJECTORIES_ADD_AGENT_ID,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "session_id",
        sql: schema::ALTER_TRAJECTORIES_ADD_SESSION_ID,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "authz_denied",
        sql: schema::ALTER_TRAJECTORIES_ADD_AUTHZ_DENIED,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "denied_resource",
        sql: schema::ALTER_TRAJECTORIES_ADD_DENIED_RESOURCE,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "denied_module",
        sql: schema::ALTER_TRAJECTORIES_ADD_DENIED_MODULE,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "source",
        sql: schema::ALTER_TRAJECTORIES_ADD_SOURCE,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "spec_governed",
        sql: schema::ALTER_TRAJECTORIES_ADD_SPEC_GOVERNED,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "request_body",
        sql: schema::ALTER_TRAJECTORIES_ADD_REQUEST_BODY,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "intent",
        sql: schema::ALTER_TRAJECTORIES_ADD_INTENT,
    },
    MigrationStep::AddColumn {
        table: "trajectories",
        column: "matched_policy_ids",
        sql: schema::ALTER_TRAJECTORIES_ADD_MATCHED_POLICY_IDS,
    },
    MigrationStep::Sql(schema::CREATE_TRAJECTORIES_SUCCESS_INDEX),
    MigrationStep::Sql(schema::CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX),
    MigrationStep::Sql(schema::CREATE_TRAJECTORIES_AGENT_INDEX),
    MigrationStep::Sql(schema::CREATE_FEATURE_REQUESTS_TABLE),
    MigrationStep::Sql(schema::CREATE_EVOLUTION_RECORDS_TABLE),
    MigrationStep::Sql(schema::CREATE_EVOLUTION_RECORDS_TYPE_INDEX),
    MigrationStep::Sql(schema::CREATE_EVOLUTION_RECORDS_STATUS_INDEX),
    MigrationStep::Sql(schema::CREATE_DESIGN_TIME_EVENTS_TABLE),
    MigrationStep::Sql(schema::CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX),
    MigrationStep::Sql(schema::CREATE_OTS_TRAJECTORIES_TABLE),
    MigrationStep::AddColumn {
        table: "ots_trajectories",
        column: "persistence_status",
        sql: schema::ALTER_OTS_TRAJECTORIES_ADD_PERSISTENCE_STATUS,
    },
    MigrationStep::AddColumn {
        table: "ots_trajectories",
        column: "persist_attempts",
        sql: schema::ALTER_OTS_TRAJECTORIES_ADD_PERSIST_ATTEMPTS,
    },
    MigrationStep::AddColumn {
        table: "ots_trajectories",
        column: "last_error",
        sql: schema::ALTER_OTS_TRAJECTORIES_ADD_LAST_ERROR,
    },
    MigrationStep::RebuildOtsTrajectories,
    MigrationStep::Sql(schema::CREATE_OTS_TRAJECTORIES_AGENT_INDEX),
    MigrationStep::Sql(schema::CREATE_OTS_TRAJECTORIES_TENANT_INDEX),
    MigrationStep::Sql(schema::CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX),
    MigrationStep::Sql(schema::CREATE_OTS_TRAJECTORIES_STATUS_INDEX),
];

const QUERY_PLANE_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_BLOBS_TABLE),
    MigrationStep::AddColumn {
        table: "blobs",
        column: "expires_at",
        sql: schema::ALTER_BLOBS_ADD_EXPIRES_AT,
    },
    MigrationStep::Sql(schema::CREATE_BLOBS_EXPIRES_AT_INDEX),
    MigrationStep::Sql(schema::CREATE_ENTITY_CATALOG_TABLE),
    MigrationStep::AddColumn {
        table: "entity_catalog",
        column: "projection_hash",
        sql: schema::ALTER_ENTITY_CATALOG_ADD_PROJECTION_HASH,
    },
    MigrationStep::AddColumn {
        table: "entity_catalog",
        column: "fields",
        sql: schema::ALTER_ENTITY_CATALOG_ADD_FIELDS,
    },
    MigrationStep::AddColumn {
        table: "entity_catalog",
        column: "state",
        sql: schema::ALTER_ENTITY_CATALOG_ADD_STATE,
    },
    MigrationStep::Sql(schema::CREATE_ENTITY_CATALOG_TYPE_INDEX),
    MigrationStep::Sql(schema::CREATE_ENTITY_CATALOG_STATUS_INDEX),
    MigrationStep::Sql(schema::CREATE_ENTITY_FIELD_INDEX_TABLE),
    MigrationStep::Sql(schema::CREATE_ENTITY_FIELD_INDEX_LOOKUP),
    MigrationStep::Sql(schema::CREATE_ENTITY_FIELD_INDEX_STATUS),
];

const DECLARED_INDEX_STEPS: &[MigrationStep] = &[
    MigrationStep::Sql(schema::CREATE_ENTITY_KEY_INDEX_TABLE),
    MigrationStep::Sql(schema::CREATE_ENTITY_KEY_INDEX_ENTITY),
    MigrationStep::Sql(schema::CREATE_ENTITY_VECTOR_INDEX_TABLE),
    MigrationStep::Sql(schema::CREATE_ENTITY_VECTOR_INDEX_PARTITION),
    MigrationStep::Sql(schema::CREATE_ENTITY_VECTOR_INDEX_ENTITY),
    MigrationStep::Sql(schema::CREATE_VECTOR_INDEX_BACKFILL_WATERMARK),
];

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "event-journal-and-snapshots",
        steps: JOURNAL_STEPS,
    },
    Migration {
        version: 2,
        name: "specs-constraints-and-integrations",
        steps: SPEC_INTEGRATION_STEPS,
    },
    Migration {
        version: 3,
        name: "authorization-and-artifacts",
        steps: AUTHORIZATION_STEPS,
    },
    Migration {
        version: 4,
        name: "apps-platform-and-secrets",
        steps: APP_PLATFORM_STEPS,
    },
    Migration {
        version: 5,
        name: "trajectories-and-evolution",
        steps: TRAJECTORY_EVOLUTION_STEPS,
    },
    Migration {
        version: 6,
        name: "blob-and-query-plane",
        steps: QUERY_PLANE_STEPS,
    },
    Migration {
        version: 7,
        name: "declared-key-and-vector-indexes",
        steps: DECLARED_INDEX_STEPS,
    },
];

#[cfg(test)]
mod tests {
    use super::super::runner::expected_checksums;

    const RELEASED_CHECKSUMS: &[&str] = &[
        "78bafc020d87a65741a6f7c117604f693d5eb265d75b178db1737f8934da8069",
        "83bc0de0ecf597a24ebe14fc6636b9b70b3cc76b6342b326afb583715e5d18b9",
        "54a077e4353c6df79dce2029cded8ce148c50be90400c4893dea21752adde4ea",
        "6dfcf2905113a7943f80c44da094cb5b53b35633298b1a8fdf933df127b1ee8d",
        "f63408461791d04d70082f996c5f7bd620d3f6af505b9c98ffa7d3a63df38d75",
        "5347da7626a3ca311ba8295e46fc7a0a22f0f6eb1944f09aee85dabca3a7fc4d",
        "a8b51d91118d03697d98db8a3ff55fbed5967a71e7305dcd13876a56ad206a7c",
    ];

    #[tokio::test]
    async fn released_migration_checksums_are_stable() {
        let checksums = expected_checksums().await.expect("expected checksums");
        assert!(checksums.len() >= RELEASED_CHECKSUMS.len());
        assert_eq!(&checksums[..RELEASED_CHECKSUMS.len()], RELEASED_CHECKSUMS);
    }
}
