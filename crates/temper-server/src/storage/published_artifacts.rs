use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::{
    PostgresEventStore, PostgresPublishedArtifactRow, PostgresPublishedArtifactUpsert,
};
use temper_store_turso::{
    PublishedArtifactRow as TursoPublishedArtifactRow,
    PublishedArtifactUpsert as TursoPublishedArtifactUpsert, TursoEventStore,
};

/// Backend-neutral PublishedArtifact read-model row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PublishedArtifactStoreRow {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedArtifactStoreUpsert {
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

impl From<TursoPublishedArtifactRow> for PublishedArtifactStoreRow {
    fn from(row: TursoPublishedArtifactRow) -> Self {
        Self {
            id: row.id,
            tenant: row.tenant,
            source_file_id: row.source_file_id,
            source_file_version_id: row.source_file_version_id,
            content_hash: row.content_hash,
            label: row.label,
            mime_type: row.mime_type,
            byte_length: row.byte_length,
            public_storage_key: row.public_storage_key,
            public_url: row.public_url,
            owner_ref_type: row.owner_ref_type,
            owner_ref_id: row.owner_ref_id,
            status: row.status,
        }
    }
}

impl From<PostgresPublishedArtifactRow> for PublishedArtifactStoreRow {
    fn from(row: PostgresPublishedArtifactRow) -> Self {
        Self {
            id: row.id,
            tenant: row.tenant,
            source_file_id: row.source_file_id,
            source_file_version_id: row.source_file_version_id,
            content_hash: row.content_hash,
            label: row.label,
            mime_type: row.mime_type,
            byte_length: row.byte_length,
            public_storage_key: row.public_storage_key,
            public_url: row.public_url,
            owner_ref_type: row.owner_ref_type,
            owner_ref_id: row.owner_ref_id,
            status: row.status,
        }
    }
}

/// Published-artifact metadata persistence capability.
#[async_trait::async_trait]
pub trait PublishedArtifactStore: Send + Sync {
    async fn upsert_published_artifact(
        &self,
        artifact: &PublishedArtifactStoreUpsert,
    ) -> Result<PublishedArtifactStoreRow, PersistenceError>;

    async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PublishedArtifactStoreRow>, PersistenceError>;
}

#[async_trait::async_trait]
impl PublishedArtifactStore for PostgresEventStore {
    async fn upsert_published_artifact(
        &self,
        artifact: &PublishedArtifactStoreUpsert,
    ) -> Result<PublishedArtifactStoreRow, PersistenceError> {
        PostgresEventStore::upsert_published_artifact(
            self,
            &PostgresPublishedArtifactUpsert {
                id: artifact.id.as_str(),
                tenant: artifact.tenant.as_str(),
                source_file_id: artifact.source_file_id.as_str(),
                source_file_version_id: artifact.source_file_version_id.as_str(),
                content_hash: artifact.content_hash.as_str(),
                label: artifact.label.as_str(),
                mime_type: artifact.mime_type.as_str(),
                byte_length: artifact.byte_length,
                public_storage_key: artifact.public_storage_key.as_str(),
                public_url: artifact.public_url.as_str(),
                owner_ref_type: artifact.owner_ref_type.as_str(),
                owner_ref_id: artifact.owner_ref_id.as_str(),
                status: artifact.status.as_str(),
            },
        )
        .await
        .map(PublishedArtifactStoreRow::from)
    }

    async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PublishedArtifactStoreRow>, PersistenceError> {
        PostgresEventStore::load_published_artifact(self, tenant, artifact_id)
            .await
            .map(|row| row.map(PublishedArtifactStoreRow::from))
    }
}

#[async_trait::async_trait]
impl PublishedArtifactStore for TursoEventStore {
    async fn upsert_published_artifact(
        &self,
        artifact: &PublishedArtifactStoreUpsert,
    ) -> Result<PublishedArtifactStoreRow, PersistenceError> {
        TursoEventStore::upsert_published_artifact(
            self,
            &TursoPublishedArtifactUpsert {
                id: artifact.id.clone(),
                tenant: artifact.tenant.clone(),
                source_file_id: artifact.source_file_id.clone(),
                source_file_version_id: artifact.source_file_version_id.clone(),
                content_hash: artifact.content_hash.clone(),
                label: artifact.label.clone(),
                mime_type: artifact.mime_type.clone(),
                byte_length: artifact.byte_length,
                public_storage_key: artifact.public_storage_key.clone(),
                public_url: artifact.public_url.clone(),
                owner_ref_type: artifact.owner_ref_type.clone(),
                owner_ref_id: artifact.owner_ref_id.clone(),
                status: artifact.status.clone(),
            },
        )
        .await
        .map(PublishedArtifactStoreRow::from)
    }

    async fn load_published_artifact(
        &self,
        tenant: &str,
        artifact_id: &str,
    ) -> Result<Option<PublishedArtifactStoreRow>, PersistenceError> {
        TursoEventStore::load_published_artifact(self, tenant, artifact_id)
            .await
            .map(|row| row.map(PublishedArtifactStoreRow::from))
    }
}
