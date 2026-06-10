//! Published artifact rows (public file snapshots).

use sqlx::Row;
use temper_runtime::persistence::PersistenceError;

use super::storage_error;
use crate::PostgresEventStore;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PostgresPublishedArtifactRow {
    pub id: String,
    pub tenant: String,
    pub source_file_id: String,
    pub source_file_version_id: String,
    pub content_hash: String,
    pub label: String,
    pub mime_type: String,
    pub byte_length: i64,
    pub public_storage_key: String,
    pub public_url: String,
    pub owner_ref_type: String,
    pub owner_ref_id: String,
    pub status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PostgresPublishedArtifactUpsert<'a> {
    pub id: &'a str,
    pub tenant: &'a str,
    pub source_file_id: &'a str,
    pub source_file_version_id: &'a str,
    pub content_hash: &'a str,
    pub label: &'a str,
    pub mime_type: &'a str,
    pub byte_length: i64,
    pub public_storage_key: &'a str,
    pub public_url: &'a str,
    pub owner_ref_type: &'a str,
    pub owner_ref_id: &'a str,
    pub status: &'a str,
}

impl PostgresEventStore {
    #[tracing::instrument(skip_all, fields(
        otel.name = "postgres.upsert_published_artifact",
        tenant = %artifact.tenant,
        artifact_label = %artifact.label,
        owner_ref_type = %artifact.owner_ref_type,
        owner_ref_id = %artifact.owner_ref_id,
    ))]
    pub async fn upsert_published_artifact(
        &self,
        artifact: &PostgresPublishedArtifactUpsert<'_>,
    ) -> Result<PostgresPublishedArtifactRow, PersistenceError> {
        crate::dbm::postgres_query!(
            "INSERT INTO published_artifacts (
                id, tenant, source_file_id, source_file_version_id, content_hash,
                label, mime_type, byte_length, public_storage_key, public_url,
                owner_ref_type, owner_ref_id, status, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, now())
            ON CONFLICT(id) DO UPDATE SET
                tenant = EXCLUDED.tenant,
                source_file_id = EXCLUDED.source_file_id,
                source_file_version_id = EXCLUDED.source_file_version_id,
                content_hash = EXCLUDED.content_hash,
                label = EXCLUDED.label,
                mime_type = EXCLUDED.mime_type,
                byte_length = EXCLUDED.byte_length,
                public_storage_key = EXCLUDED.public_storage_key,
                public_url = EXCLUDED.public_url,
                owner_ref_type = EXCLUDED.owner_ref_type,
                owner_ref_id = EXCLUDED.owner_ref_id,
                status = EXCLUDED.status,
                updated_at = now()",
        )
        .bind(artifact.id)
        .bind(artifact.tenant)
        .bind(artifact.source_file_id)
        .bind(artifact.source_file_version_id)
        .bind(artifact.content_hash)
        .bind(artifact.label)
        .bind(artifact.mime_type)
        .bind(artifact.byte_length)
        .bind(artifact.public_storage_key)
        .bind(artifact.public_url)
        .bind(artifact.owner_ref_type)
        .bind(artifact.owner_ref_id)
        .bind(artifact.status)
        .execute(self.pool())
        .await
        .map_err(storage_error)?;

        self.load_published_artifact(artifact.tenant, artifact.id)
            .await?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "published artifact '{}' was not readable after upsert",
                    artifact.id
                ))
            })
    }

    #[tracing::instrument(skip_all, fields(
        otel.name = "postgres.load_published_artifact",
        tenant,
        artifact_id,
    ))]
    pub async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PostgresPublishedArtifactRow>, PersistenceError> {
        let row = crate::dbm::postgres_query!(
            "SELECT id, tenant, source_file_id, source_file_version_id, content_hash,
                    label, mime_type, byte_length, public_storage_key, public_url,
                    owner_ref_type, owner_ref_id, status
               FROM published_artifacts
              WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(artifact_id)
        .fetch_optional(self.pool())
        .await
        .map_err(storage_error)?;
        Ok(row.map(row_to_published_artifact))
    }
}

fn row_to_published_artifact(row: sqlx::postgres::PgRow) -> PostgresPublishedArtifactRow {
    PostgresPublishedArtifactRow {
        id: row.get("id"),
        tenant: row.get("tenant"),
        source_file_id: row.get("source_file_id"),
        source_file_version_id: row.get("source_file_version_id"),
        content_hash: row.get("content_hash"),
        label: row.get("label"),
        mime_type: row.get("mime_type"),
        byte_length: row.get("byte_length"),
        public_storage_key: row.get("public_storage_key"),
        public_url: row.get("public_url"),
        owner_ref_type: row.get("owner_ref_type"),
        owner_ref_id: row.get("owner_ref_id"),
        status: row.get("status"),
    }
}
