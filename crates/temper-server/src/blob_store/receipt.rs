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
            temper_runtime::persistence::StreamDescriptorInputV1 {
                subject,
                authorization_parent,
                content_hash: self.content_hash,
                storage: self.storage,
                byte_length: self.byte_length,
                content_type: self.content_type,
                content_event_sequence: event_sequence,
                descriptor_event_sequence: event_sequence,
                mutability,
            },
        )
        .map_err(|error| error.to_string())
    }
}
