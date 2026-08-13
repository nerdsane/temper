use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Artifact-bound compatibility evidence for one pinned closure change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSdkCompatibilityProof {
    /// Closure digest represented by the compiled SDK.
    pub prior_closure_digest: String,
    /// Closure digest resolved for the candidate activation.
    pub candidate_closure_digest: String,
    /// Independently recomputable semantic hashes from the compiled manifest.
    pub prior_used_symbol_hashes: BTreeMap<String, String>,
    /// Independently recomputable semantic hashes from the candidate closure.
    pub candidate_used_symbol_hashes: BTreeMap<String, String>,
    /// Grant digest represented by the compiled SDK.
    pub prior_grant_digest: String,
    /// Equal-or-narrower candidate grant digest.
    pub candidate_grant_digest: String,
}
