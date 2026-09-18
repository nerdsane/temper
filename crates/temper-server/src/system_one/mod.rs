//! Recorded System One evaluations used as pre-transition IOA evidence.

mod attempts;
mod evidence;
pub mod provider;
mod receipts;
mod resolve;

pub(crate) use attempts::{AttemptInput, bind_attempt, check_attempt};
pub use evidence::SystemOneEvidence;
pub(crate) use evidence::{collect_guards, table_digest};
#[cfg(test)]
pub(crate) use receipts::EvaluationReceipt;
pub(crate) use receipts::{ReceiptBinding, recorded_receipt, resolve_receipt};
pub(crate) use resolve::{SystemOneResolution, canonical_principal_identity};

#[cfg(test)]
#[path = "dst_test.rs"]
mod dst;

#[cfg(test)]
mod actor_tests;
