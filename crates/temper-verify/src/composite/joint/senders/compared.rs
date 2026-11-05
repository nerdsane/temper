//! What Cedar compares, of what a question to it can hold a made-up value in.

use temper_authz::Read;
use temper_authz::platform_keys::{CAUSE_ENTITY_ID, TRIGGER_SOURCE_ENTITY_ID};

use super::Senders;

impl Senders {
    /// What each comparison in the policies reads that a question here can
    /// answer with a made-up value: a record's fields, and the two context
    /// keys that name a record (`symbol`).
    pub(in crate::composite::joint) fn compared(&self) -> Vec<Vec<Read>> {
        let Some(policies) = &self.policies else {
            return Vec::new();
        };
        policies
            .compared()
            .into_iter()
            .map(|reads| {
                reads
                    .into_iter()
                    .filter(|read| match read {
                        Read::Resource(_) => true,
                        Read::Context(key) => {
                            matches!(key.as_str(), TRIGGER_SOURCE_ENTITY_ID | CAUSE_ENTITY_ID)
                        }
                    })
                    .collect()
            })
            .collect()
    }
}
