//! A File's body, staged on disk as it arrives and then sent on to the object store as it is, so a large upload
//! never sits in memory and its key (its sha256) is known before the object store sees a byte.

use bytes::Bytes;
use tokio::io::AsyncReadExt as _;

use super::{
    BlobByteStream, BlobIngestPermit, BlobIngestProgressPolicy, BlobStageError, STREAM_CHUNK_BYTES,
    StagedBlob,
};
use crate::blob_store::BlobStore;

impl StagedBlob {
    /// `sha256:<hex>` of the staged body, as a File records it.
    pub(crate) fn content_hash(&self) -> String {
        format!("sha256:{}", self.content_sha256)
    }

    pub(crate) fn len(&self) -> usize {
        self.declared_len
    }

    fn raw_stream(&self) -> BlobByteStream {
        let path = self.path.to_path_buf();
        Box::pin(async_stream::try_stream! {
            let mut file = tokio::fs::File::open(&path).await?;
            let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
            loop {
                let read = file.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                yield Bytes::copy_from_slice(&buffer[..read]);
            }
        })
    }
}

impl BlobStore {
    /// Stage a File's body of exactly `declared_len` bytes, hashing it on the way.
    pub(crate) async fn stage_file_stream(
        &self,
        stream: BlobByteStream,
        declared_len: usize,
        progress: &BlobIngestProgressPolicy,
        admission: &mut BlobIngestPermit,
    ) -> Result<StagedBlob, BlobStageError> {
        self.stage_canonical_stream(stream, declared_len, &[], progress, admission)
            .await
    }

    /// Send a staged File's body to `key`, streamed from disk.
    pub(crate) async fn put_staged_file(
        &self,
        key: &str,
        staged: &StagedBlob,
    ) -> Result<(), String> {
        self.put_content_addressed_stream(key, staged.raw_stream(), staged.declared_len as u64)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::stream;
    use temper_runtime::tenant::TenantId;

    use super::*;
    use crate::blob_store::BlobIngestBudget;

    #[tokio::test]
    async fn a_staged_file_is_keyed_by_its_sha256_and_stored_byte_for_byte() {
        let dir = tempfile::tempdir().expect("dir");
        let store = BlobStore::local_fs(dir.path());
        let budget = BlobIngestBudget::files();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let chunks: Vec<Result<Bytes, std::io::Error>> = body
            .chunks(70_000)
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        let mut permit = budget
            .try_reserve(&TenantId::default(), body.len())
            .expect("admitted");
        let staged = store
            .stage_file_stream(
                Box::pin(stream::iter(chunks)),
                body.len(),
                budget.progress_policy(),
                &mut permit,
            )
            .await
            .expect("staged");
        let expected = format!("sha256:{}", crate::aws_sigv4::sha256_hex(&body));
        assert_eq!(staged.content_hash(), expected);
        assert_eq!(staged.len(), body.len());
        let key = format!("temper-fs/{expected}");
        store.put_staged_file(&key, &staged).await.expect("stored");
        let stored = tokio::time::timeout(Duration::from_secs(10), store.get(&key))
            .await
            .expect("in time")
            .expect("read")
            .expect("present");
        assert_eq!(stored, body);
    }

    #[test]
    fn several_files_of_one_tenant_may_come_in_at_once() {
        let budget = BlobIngestBudget::files();
        let tenant = TenantId::default();
        let held: Vec<_> = (0..8)
            .map(|_| budget.try_reserve(&tenant, 1024).expect("admitted"))
            .collect();
        assert_eq!(held.len(), 8);
        assert!(
            budget.try_reserve(&tenant, 1024).is_err(),
            "a ninth waits its turn"
        );
    }
}
