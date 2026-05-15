use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PublishedArtifactRow {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedArtifactUpsert {
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

impl TursoEventStore {
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_published_artifact",
        tenant = %artifact.tenant,
        artifact_label = %artifact.label,
        owner_ref_type = %artifact.owner_ref_type,
        owner_ref_id = %artifact.owner_ref_id,
    ))]
    pub async fn upsert_published_artifact(
        &self,
        artifact: &PublishedArtifactUpsert,
    ) -> Result<PublishedArtifactRow, PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO published_artifacts (
                id, tenant, source_file_id, source_file_version_id, content_hash,
                label, mime_type, byte_length, public_storage_key, public_url,
                owner_ref_type, owner_ref_id, status, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                source_file_id = excluded.source_file_id,
                source_file_version_id = excluded.source_file_version_id,
                content_hash = excluded.content_hash,
                label = excluded.label,
                mime_type = excluded.mime_type,
                byte_length = excluded.byte_length,
                public_storage_key = excluded.public_storage_key,
                public_url = excluded.public_url,
                owner_ref_type = excluded.owner_ref_type,
                owner_ref_id = excluded.owner_ref_id,
                status = excluded.status,
                updated_at = datetime('now')",
            params![
                artifact.id.as_str(),
                artifact.tenant.as_str(),
                artifact.source_file_id.as_str(),
                artifact.source_file_version_id.as_str(),
                artifact.content_hash.as_str(),
                artifact.label.as_str(),
                artifact.mime_type.as_str(),
                artifact.byte_length,
                artifact.public_storage_key.as_str(),
                artifact.public_url.as_str(),
                artifact.owner_ref_type.as_str(),
                artifact.owner_ref_id.as_str(),
                artifact.status.as_str(),
            ],
        )
        .await
        .map_err(storage_error)?;

        self.load_published_artifact(&artifact.tenant, &artifact.id)
            .await?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "published artifact '{}' was not readable after upsert",
                    artifact.id
                ))
            })
    }

    #[instrument(skip_all, fields(
        otel.name = "turso.load_published_artifact",
        tenant,
        artifact_id,
    ))]
    pub async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PublishedArtifactRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, tenant, source_file_id, source_file_version_id, content_hash,
                        label, mime_type, byte_length, public_storage_key, public_url,
                        owner_ref_type, owner_ref_id, status
                   FROM published_artifacts
                  WHERE tenant = ?1 AND id = ?2",
                params![tenant, artifact_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Ok(Some(PublishedArtifactRow {
            id: row.get(0).map_err(storage_error)?,
            tenant: row.get(1).map_err(storage_error)?,
            source_file_id: row.get(2).map_err(storage_error)?,
            source_file_version_id: row.get(3).map_err(storage_error)?,
            content_hash: row.get(4).map_err(storage_error)?,
            label: row.get(5).map_err(storage_error)?,
            mime_type: row.get(6).map_err(storage_error)?,
            byte_length: row.get(7).map_err(storage_error)?,
            public_storage_key: row.get(8).map_err(storage_error)?,
            public_url: row.get(9).map_err(storage_error)?,
            owner_ref_type: row.get(10).map_err(storage_error)?,
            owner_ref_id: row.get(11).map_err(storage_error)?,
            status: row.get(12).map_err(storage_error)?,
        }))
    }
}
