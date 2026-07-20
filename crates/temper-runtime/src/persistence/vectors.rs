//! Declared entity key and vector index value types.

/// A declared-key row to co-commit with an append (ADR-0153). The entity claims
/// `key_hash` for `key_name`; the store writes it into `entity_key_index` in the
/// same transaction as the journal append, giving the read plane an `O(log n)`
/// present/absent probe (the negative-existence access path, ARN-68).
#[derive(Debug, Clone)]
pub struct EntityKeyRow {
    /// The declared key's identifier (the `[[key]]` block's `name`).
    pub key_name: String,
    /// The canonical, type-tagged hash of the key's values.
    pub key_hash: String,
}

/// A derived vector-index row to co-commit with an append (ADR-0155). Parsed from
/// the entity's post-transition state for one declared `[[vector]]` path: the
/// float vector and the model tag that partitions its space. Stores that maintain
/// `entity_vector_index` write one row per `(decl_name, model_tag, entity_id)`; the
/// blob is packed little-endian f32. Unlike a key row this has no uniqueness
/// constraint — it is derived, rebuildable ranking state.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorRow {
    /// The declared vector path's identifier (the `[[vector]]` block's `name`).
    pub decl_name: String,
    /// The model tag that partitions this vector's space (only same-tag vectors
    /// are ever compared).
    pub model_tag: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}

/// Pack an `f32` slice to little-endian bytes — the `entity_vector_index` blob
/// encoding shared by every backend (ADR-0155). Kept here beside [`EntityVectorRow`]
/// so the stores and the kernel ranking agree on the byte layout.
pub fn pack_f32_le(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Unpack little-endian bytes back to `f32`. `None` if the byte length is not a
/// multiple of 4, or if any component is not finite (both signal a corrupt blob),
/// so a bad row is skipped rather than panicking or feeding a `NaN`/`inf` into the
/// kNN ranking — where a `NaN` would sort ahead of every real score.
pub fn unpack_f32_le(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

/// One candidate row returned from the vector index for a kNN read (ADR-0155):
/// an entity and its packed vector for one `(tenant, type, decl, model_tag)`
/// partition. The kernel — not the store — computes the metric over these in the
/// store-supplied (entity-id) order, so ranking is identical across backends.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorCandidate {
    /// The entity holding this vector.
    pub entity_id: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}
