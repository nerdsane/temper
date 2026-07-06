//! Vector access-path packing, parsing, and exact-scan kNN ranking (ADR-0155).
//!
//! A declared `[[vector]]` path indexes a float vector per entity, partitioned by
//! a model tag. The store supplies candidate `(entity_id, vector)` rows in a fixed
//! (entity-id) order; this module computes the metric and ranks them. Ranking
//! lives here — in the kernel, once — rather than in each storage backend, so the
//! result is identical on sim, Postgres, and Turso (the property the DST asserts).
//!
//! The ranking is **deterministic**: no clock, no randomness, no map iteration; f32
//! accumulation proceeds in the store's candidate order and ties break by entity
//! id. This is what makes kernel-side similarity admissible under deterministic
//! simulation where app-side similarity never was.

use temper_runtime::persistence::EntityVectorCandidate;
// The blob encoders live beside `EntityVectorRow` in temper-runtime so every store
// and the kernel ranking share one byte layout; re-exported here for callers that
// reach for them through the vector-index module.
pub use temper_runtime::persistence::{pack_f32_le, unpack_f32_le};

/// The stable signature of a type's declared vector-path set: the sorted,
/// comma-joined path NAMES (ADR-0155). Recorded in the vector-index backfill
/// watermark and compared on the next backfill, so declaring an ADDITIONAL vector
/// path (a changed signature) re-indexes the type instead of being treated as
/// already complete. Deterministic (sorted, no map iteration). Mirrors
/// `declared_key_set_signature`.
pub fn declared_vector_set_signature(
    vectors: &[temper_jit::table::types::DeclaredVector],
) -> String {
    let mut names: Vec<&str> = vectors.iter().map(|v| v.name.as_str()).collect();
    names.sort();
    names.join(",")
}

/// The similarity metric declared on a `[[vector]]` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    /// Cosine similarity — the dot product normalized by both magnitudes.
    Cosine,
    /// Raw dot product.
    Dot,
    /// Euclidean (L2) distance. Exposed as a closeness (negated) so nearest is
    /// always the largest score, uniform with cosine/dot.
    L2,
}

impl VectorMetric {
    /// Parse the declared metric string. `None` for an unknown metric (the spec
    /// cascade already rejects those, so this only guards a corrupt table).
    pub fn parse(metric: &str) -> Option<Self> {
        match metric {
            "cosine" => Some(Self::Cosine),
            "dot" => Some(Self::Dot),
            "l2" => Some(Self::L2),
            _ => None,
        }
    }
}

/// Parse an entity's vector property into exactly `dims` `f32`s.
///
/// Accepts either a JSON array of numbers (`[0.1, 0.2, …]`) or a JSON **string**
/// containing such an array (`"[0.1, 0.2, …]"`) — producers may store the vector
/// either way. Returns `None` on any mismatch (wrong length, a non-numeric
/// element, an unparseable string), in which case the entity is simply not indexed
/// for this path — the same posture as an incomplete declared key. Non-finite
/// components (`NaN`/`inf`) also decline, so ranking never sees a value that has no
/// total order.
pub fn parse_vector_property(value: &serde_json::Value, dims: usize) -> Option<Vec<f32>> {
    let array = match value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::String(text) => match serde_json::from_str(text).ok()? {
            serde_json::Value::Array(items) => items,
            _ => return None,
        },
        _ => return None,
    };
    if array.len() != dims {
        return None;
    }
    let mut out = Vec::with_capacity(dims);
    for item in &array {
        let component = item.as_f64()? as f32;
        if !component.is_finite() {
            return None;
        }
        out.push(component);
    }
    Some(out)
}

/// One ranked entity plus its closeness score (higher = nearer).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredEntity {
    pub entity_id: String,
    pub score: f32,
}

/// The closeness score for `query` vs `candidate` under `metric` — higher is
/// nearer for every metric: cosine similarity, dot product, and **negated** L2
/// distance. f32 accumulation proceeds in index order. `None` if the lengths
/// differ (a corrupt row); a zero-magnitude vector scores 0 under cosine rather
/// than producing a `NaN`.
fn closeness(metric: VectorMetric, query: &[f32], candidate: &[f32]) -> Option<f32> {
    if query.len() != candidate.len() {
        return None;
    }
    match metric {
        VectorMetric::Dot => {
            let mut dot = 0.0f32;
            for (q, c) in query.iter().zip(candidate.iter()) {
                dot += q * c;
            }
            Some(dot)
        }
        VectorMetric::Cosine => {
            let mut dot = 0.0f32;
            let mut q_norm = 0.0f32;
            let mut c_norm = 0.0f32;
            for (q, c) in query.iter().zip(candidate.iter()) {
                dot += q * c;
                q_norm += q * q;
                c_norm += c * c;
            }
            let denom = q_norm.sqrt() * c_norm.sqrt();
            if denom == 0.0 {
                Some(0.0)
            } else {
                Some(dot / denom)
            }
        }
        VectorMetric::L2 => {
            let mut sum_sq = 0.0f32;
            for (q, c) in query.iter().zip(candidate.iter()) {
                let d = q - c;
                sum_sq += d * d;
            }
            // Negated so nearest (smallest distance) is the largest score.
            Some(-sum_sq.sqrt())
        }
    }
}

/// Rank `candidates` nearest-first and return at most `k`.
///
/// Order is (score descending, then entity id ascending) — a total, deterministic
/// order under every seed. `exclude` drops one entity id (the reference entity when
/// the query came from `to=<id>`, so it is never its own top result). Candidates
/// whose length does not match `query` are skipped (corrupt rows never rank).
pub fn rank_nearest(
    metric: VectorMetric,
    query: &[f32],
    candidates: &[EntityVectorCandidate],
    k: usize,
    exclude: Option<&str>,
) -> Vec<ScoredEntity> {
    let mut scored: Vec<ScoredEntity> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if exclude == Some(candidate.entity_id.as_str()) {
            continue;
        }
        if let Some(score) = closeness(metric, query, &candidate.vector) {
            scored.push(ScoredEntity {
                entity_id: candidate.entity_id.clone(),
                score,
            });
        }
    }
    // Deterministic total order: score desc (via total_cmp so there is no
    // NaN-induced ambiguity), then entity id asc as the tiebreak.
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    scored.truncate(k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, vector: Vec<f32>) -> EntityVectorCandidate {
        EntityVectorCandidate {
            entity_id: id.to_string(),
            vector,
        }
    }

    #[test]
    fn pack_unpack_roundtrips() {
        let v = vec![0.0f32, 1.5, -2.25, 384.0];
        assert_eq!(unpack_f32_le(&pack_f32_le(&v)).unwrap(), v);
    }

    #[test]
    fn unpack_rejects_misaligned_bytes() {
        assert!(unpack_f32_le(&[1, 2, 3]).is_none());
    }

    #[test]
    fn parse_accepts_array_and_string_forms() {
        let dims = 3;
        let from_array = parse_vector_property(&serde_json::json!([1.0, 2.0, 3.0]), dims).unwrap();
        let from_string =
            parse_vector_property(&serde_json::json!("[1.0, 2.0, 3.0]"), dims).unwrap();
        assert_eq!(from_array, vec![1.0, 2.0, 3.0]);
        assert_eq!(from_string, from_array);
    }

    #[test]
    fn parse_rejects_wrong_dims_and_nonnumeric() {
        assert!(parse_vector_property(&serde_json::json!([1.0, 2.0]), 3).is_none());
        assert!(parse_vector_property(&serde_json::json!([1.0, "x", 3.0]), 3).is_none());
        assert!(parse_vector_property(&serde_json::json!("not json"), 3).is_none());
        assert!(parse_vector_property(&serde_json::json!(42), 3).is_none());
    }

    #[test]
    fn cosine_ranks_most_similar_first() {
        let query = vec![1.0, 0.0];
        let candidates = vec![
            candidate("orthogonal", vec![0.0, 1.0]),
            candidate("same", vec![2.0, 0.0]),
            candidate("opposite", vec![-1.0, 0.0]),
        ];
        let ranked = rank_nearest(VectorMetric::Cosine, &query, &candidates, 3, None);
        assert_eq!(ranked[0].entity_id, "same");
        assert_eq!(ranked[1].entity_id, "orthogonal");
        assert_eq!(ranked[2].entity_id, "opposite");
    }

    #[test]
    fn l2_ranks_nearest_first_and_excludes_self() {
        let query = vec![0.0, 0.0];
        let candidates = vec![
            candidate("self", vec![0.0, 0.0]),
            candidate("near", vec![1.0, 0.0]),
            candidate("far", vec![5.0, 5.0]),
        ];
        let ranked = rank_nearest(VectorMetric::L2, &query, &candidates, 10, Some("self"));
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].entity_id, "near");
        assert_eq!(ranked[1].entity_id, "far");
    }

    #[test]
    fn ties_break_by_entity_id_ascending() {
        let query = vec![1.0, 0.0];
        // Two candidates with identical vectors -> identical score -> id tiebreak.
        let candidates = vec![
            candidate("b", vec![1.0, 0.0]),
            candidate("a", vec![1.0, 0.0]),
        ];
        let ranked = rank_nearest(VectorMetric::Dot, &query, &candidates, 2, None);
        assert_eq!(ranked[0].entity_id, "a");
        assert_eq!(ranked[1].entity_id, "b");
    }

    #[test]
    fn k_caps_result_length() {
        let query = vec![1.0];
        let candidates: Vec<_> = (0..10)
            .map(|i| candidate(&format!("e{i:02}"), vec![i as f32]))
            .collect();
        let ranked = rank_nearest(VectorMetric::Dot, &query, &candidates, 3, None);
        assert_eq!(ranked.len(), 3);
    }

    #[test]
    fn zero_magnitude_cosine_scores_zero_not_nan() {
        let query = vec![0.0, 0.0];
        let candidates = vec![candidate("x", vec![1.0, 1.0])];
        let ranked = rank_nearest(VectorMetric::Cosine, &query, &candidates, 1, None);
        assert_eq!(ranked[0].score, 0.0);
    }
}
