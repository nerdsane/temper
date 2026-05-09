use libsql::params;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PublishedAssetRow {
    pub id: String,
    pub tenant: String,
    pub source_file_id: String,
    pub source_file_version_id: String,
    pub content_hash: String,
    pub kind: String,
    pub mime_type: String,
    pub byte_length: i64,
    pub public_storage_key: String,
    pub public_url: String,
    pub owner_entity_type: String,
    pub owner_entity_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedAssetUpsert {
    pub id: String,
    pub tenant: String,
    pub source_file_id: String,
    pub source_file_version_id: String,
    pub content_hash: String,
    pub kind: String,
    pub mime_type: String,
    pub byte_length: i64,
    pub public_storage_key: String,
    pub public_url: String,
    pub owner_entity_type: String,
    pub owner_entity_id: String,
    pub status: String,
}

impl TursoEventStore {
    #[instrument(skip_all, fields(
        otel.name = "turso.upsert_published_asset",
        tenant = %asset.tenant,
        kind = %asset.kind,
        owner_entity_type = %asset.owner_entity_type,
        owner_entity_id = %asset.owner_entity_id,
    ))]
    pub async fn upsert_published_asset(
        &self,
        asset: &PublishedAssetUpsert,
    ) -> Result<PublishedAssetRow, PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO published_assets (
                id, tenant, source_file_id, source_file_version_id, content_hash,
                kind, mime_type, byte_length, public_storage_key, public_url,
                owner_entity_type, owner_entity_id, status, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                source_file_id = excluded.source_file_id,
                source_file_version_id = excluded.source_file_version_id,
                content_hash = excluded.content_hash,
                kind = excluded.kind,
                mime_type = excluded.mime_type,
                byte_length = excluded.byte_length,
                public_storage_key = excluded.public_storage_key,
                public_url = excluded.public_url,
                owner_entity_type = excluded.owner_entity_type,
                owner_entity_id = excluded.owner_entity_id,
                status = excluded.status,
                updated_at = datetime('now')",
            params![
                asset.id.as_str(),
                asset.tenant.as_str(),
                asset.source_file_id.as_str(),
                asset.source_file_version_id.as_str(),
                asset.content_hash.as_str(),
                asset.kind.as_str(),
                asset.mime_type.as_str(),
                asset.byte_length,
                asset.public_storage_key.as_str(),
                asset.public_url.as_str(),
                asset.owner_entity_type.as_str(),
                asset.owner_entity_id.as_str(),
                asset.status.as_str(),
            ],
        )
        .await
        .map_err(storage_error)?;

        self.load_published_asset(&asset.tenant, &asset.id)
            .await?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "published asset '{}' was not readable after upsert",
                    asset.id
                ))
            })
    }

    #[instrument(skip_all, fields(
        otel.name = "turso.load_published_asset",
        tenant,
        asset_id,
    ))]
    pub async fn load_published_asset(
        &self,
        tenant: &str,
        asset_id: &str,
    ) -> Result<Option<PublishedAssetRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, tenant, source_file_id, source_file_version_id, content_hash,
                        kind, mime_type, byte_length, public_storage_key, public_url,
                        owner_entity_type, owner_entity_id, status
                   FROM published_assets
                  WHERE tenant = ?1 AND id = ?2",
                params![tenant, asset_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        Ok(Some(PublishedAssetRow {
            id: row.get(0).map_err(storage_error)?,
            tenant: row.get(1).map_err(storage_error)?,
            source_file_id: row.get(2).map_err(storage_error)?,
            source_file_version_id: row.get(3).map_err(storage_error)?,
            content_hash: row.get(4).map_err(storage_error)?,
            kind: row.get(5).map_err(storage_error)?,
            mime_type: row.get(6).map_err(storage_error)?,
            byte_length: row.get(7).map_err(storage_error)?,
            public_storage_key: row.get(8).map_err(storage_error)?,
            public_url: row.get(9).map_err(storage_error)?,
            owner_entity_type: row.get(10).map_err(storage_error)?,
            owner_entity_id: row.get(11).map_err(storage_error)?,
            status: row.get(12).map_err(storage_error)?,
        }))
    }
}
