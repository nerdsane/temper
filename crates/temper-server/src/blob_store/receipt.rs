//! Host-attested receipt for committed stream bytes.

/// Invocation-local proof minted only after the host blob boundary accepts bytes.
#[derive(Debug)]
pub(crate) struct CommittedStreamReceiptV1 {
    pub(super) storage: temper_runtime::persistence::StreamStorageRefV1,
    pub(super) byte_length: u64,
    pub(super) content_hash: String,
    pub(super) content_type: Option<String>,
}

impl CommittedStreamReceiptV1 {
    pub(crate) fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub(crate) fn byte_length(&self) -> u64 {
        self.byte_length
    }

    pub(crate) fn into_descriptor(
        self,
        subject: temper_runtime::persistence::StreamEntityRef,
        authorization_parent: Option<temper_runtime::persistence::StreamEntityRef>,
        event_sequence: u64,
        mutability: temper_runtime::persistence::StreamMutability,
    ) -> Result<temper_runtime::persistence::StreamDescriptorV1, String> {
        temper_runtime::persistence::StreamDescriptorV1::new(
            subject,
            authorization_parent,
            self.content_hash,
            self.storage,
            self.byte_length,
            self.content_type,
            event_sequence,
            event_sequence,
            mutability,
        )
        .map_err(|error| error.to_string())
    }
}
