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

use temper_runtime::persistence::{EntityVectorCandidate, EntityVectorRow};
// The blob encoders live beside `EntityVectorRow` in temper-runtime so every store
// and the kernel ranking share one byte layout; re-exported here for callers that
// reach for them through the vector-index module.
pub use temper_runtime::persistence::{pack_f32_le, unpack_f32_le};

/// The stable, protocol-revisioned signature of a type's declared vector-path set
/// (ADR-0155/ADR-0171): each path is rendered as
/// `name:property:model_property:dims:metric`, sorted by name, and semicolon-joined.
/// Recorded in the vector-index backfill watermark and compared on the next
/// backfill, so ANY declaration change re-indexes the type. The protocol prefix
/// deliberately invalidates pre-ADR-0171 watermarks once, forcing every legacy row
/// through sequence-aware reconciliation. Including `dims` matters: an edited
/// `dims` makes every existing row the wrong length (they would be dropped at read
/// time as corrupt), so the type must be re-embedded/reconciled. Deterministic
/// (sorted, no map iteration). Mirrors `declared_key_set_signature`.
pub fn declared_vector_set_signature(
    vectors: &[temper_jit::table::types::DeclaredVector],
) -> String {
    const RECONCILIATION_PROTOCOL_REVISION: &str = "v2";
    let mut entries: Vec<String> = vectors
        .iter()
        .map(|v| {
            format!(
                "{}:{}:{}:{}:{}",
                v.name, v.property, v.model_property, v.dims, v.metric
            )
        })
        .collect();
    entries.sort();
    format!("{RECONCILIATION_PROTOCOL_REVISION}|{}", entries.join(";"))
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

/// Derive the complete vector-index row set from one post-transition entity state.
///
/// This is shared by single-entity appends, composite batch appends, and background
/// reconciliation so every journal-writing path interprets declarations identically.
/// A deleted state always yields an empty set; callers still pass
/// `reconcile_vectors = true` when declarations exist so that empty set purges stale
/// candidates while retaining the entity fence (ADR-0171).
pub(crate) fn rows_for_entity_state(
    vectors: &[temper_jit::table::types::DeclaredVector],
    status: &str,
    fields: &serde_json::Value,
) -> Vec<EntityVectorRow> {
    if status == "Deleted" {
        return Vec::new();
    }
    let Some(field_map) = fields.as_object() else {
        return Vec::new();
    };
    vectors
        .iter()
        .filter_map(|decl| {
            let vector = field_map
                .get(&decl.property)
                .and_then(|value| parse_vector_property(value, decl.dims))?;
            let model_tag = field_map
                .get(&decl.model_property)
                .and_then(|value| value.as_str())
                .filter(|tag| !tag.is_empty())?;
            Some(EntityVectorRow {
                decl_name: decl.name.clone(),
                model_tag: model_tag.to_string(),
                vector,
            })
        })
        .collect()
}

/// One ranked entity plus its closeness score (higher = nearer).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredEntity {
    pub entity_id: String,
    pub score: f32,
}

/// The closeness score for `query` vs `candidate` under `metric` — higher is
/// nearer for every metric: cosine similarity, dot product, and **negated** L2
/// distance.
///
/// Accumulation is in **f64** in fixed index order, then narrowed to f32. f64 has
/// the range to hold the sums of squares/products of finite f32 inputs without
/// overflowing to `inf` — an f32 accumulator can overflow, and the resulting
/// `inf/inf = NaN` sorts ABOVE every real score (`total_cmp`), so a pair of large
/// but finite vectors would rank first. Doing the math in f64 keeps the order
/// still deterministic (fixed order, f64 is exact enough and identical across
/// backends). Returns `None` if the lengths differ (a corrupt row) or the score is
/// not finite; a zero-magnitude vector scores 0 under cosine rather than `NaN`.
fn closeness(metric: VectorMetric, query: &[f32], candidate: &[f32]) -> Option<f32> {
    if query.len() != candidate.len() {
        return None;
    }
    let score: f64 = match metric {
        VectorMetric::Dot => {
            let mut dot = 0.0f64;
            for (q, c) in query.iter().zip(candidate.iter()) {
                dot += f64::from(*q) * f64::from(*c);
            }
            dot
        }
        VectorMetric::Cosine => {
            let mut dot = 0.0f64;
            let mut q_norm = 0.0f64;
            let mut c_norm = 0.0f64;
            for (q, c) in query.iter().zip(candidate.iter()) {
                let (q, c) = (f64::from(*q), f64::from(*c));
                dot += q * c;
                q_norm += q * q;
                c_norm += c * c;
            }
            let denom = q_norm.sqrt() * c_norm.sqrt();
            if denom == 0.0 { 0.0 } else { dot / denom }
        }
        VectorMetric::L2 => {
            let mut sum_sq = 0.0f64;
            for (q, c) in query.iter().zip(candidate.iter()) {
                let d = f64::from(*q) - f64::from(*c);
                sum_sq += d * d;
            }
            // Negated so nearest (smallest distance) is the largest score.
            -sum_sq.sqrt()
        }
    };
    // Narrow to f32, then require the NARROWED value to be finite — this catches
    // both a NaN/inf f64 and an f64 that is finite but outside f32 range (which
    // narrows to inf). Either way a non-finite score declines the row, so NaN/inf
    // can never rank (a NaN sorts ahead of every real score under total_cmp).
    let narrowed = score as f32;
    if narrowed.is_finite() {
        Some(narrowed)
    } else {
        None
    }
}

/// Rank `candidates` nearest-first and return at most `k`.
///
/// Order is (score descending, then entity id ascending) — a total, deterministic
/// order under every seed. `exclude` drops one entity id (the reference entity when
/// the query came from `to=<id>`, so it is never its own top result). Candidates
/// whose length does not match `query`, or whose score is not finite, are skipped —
/// a corrupt or overflowing row never ranks (a NaN would otherwise sort first).
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
    fn empty_vector_set_signature_is_protocol_revisioned() {
        assert_eq!(declared_vector_set_signature(&[]), "v2|");
    }

    #[test]
    fn remove_all_then_readd_identical_vector_changes_watermark_each_time() {
        let declaration = temper_jit::table::types::DeclaredVector {
            name: "embed".to_string(),
            property: "vector".to_string(),
            model_property: "model".to_string(),
            dims: 2,
            metric: "cosine".to_string(),
        };
        let declared = declared_vector_set_signature(std::slice::from_ref(&declaration));
        let removed = declared_vector_set_signature(&[]);
        let readded = declared_vector_set_signature(&[declaration]);

        assert_ne!(declared, removed);
        assert_eq!(declared, readded);
        assert_ne!(removed, readded);
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

    #[test]
    fn large_finite_vectors_do_not_overflow_to_nan_or_rank_first() {
        // Under cosine (bounded [-1,1]) even huge-magnitude vectors rank by direction,
        // never producing a NaN that would sort ahead of a genuinely-similar row.
        let query = vec![1.0f32, 0.0];
        let candidates = vec![
            candidate("huge_same_dir", vec![f32::MAX, 0.0]),
            candidate("near", vec![1.0, 0.01]),
            candidate("huge_orthogonal", vec![0.0, f32::MAX]),
        ];
        let ranked = rank_nearest(VectorMetric::Cosine, &query, &candidates, 3, None);
        assert!(
            ranked.iter().all(|r| r.score.is_finite()),
            "no NaN/inf scores"
        );
        // The huge same-direction vector (cosine 1.0) is nearest; the huge orthogonal
        // one (cosine 0) is last — magnitude does not let a row jump the ranking.
        assert_eq!(ranked[0].entity_id, "huge_same_dir");
        assert_eq!(ranked.last().unwrap().entity_id, "huge_orthogonal");
    }

    #[test]
    fn dot_overflow_declines_row_rather_than_ranking_inf_first() {
        // A dot product that overflows f32 range narrows to inf and is dropped, so it
        // cannot sort ahead of a finite, genuinely-scored row. The query is moderate
        // so only the pathological candidate overflows (its dot ~2e60 exceeds f32
        // range), while the finite one (dot ~2e30) is well within range.
        let query = vec![1e30f32, 1e30];
        let candidates = vec![
            candidate("overflows", vec![1e30, 1e30]),
            candidate("finite", vec![1.0, 1.0]),
        ];
        let ranked = rank_nearest(VectorMetric::Dot, &query, &candidates, 5, None);
        assert!(ranked.iter().all(|r| r.score.is_finite()));
        assert!(
            ranked.iter().all(|r| r.entity_id != "overflows"),
            "an overflowing (non-finite) score must be dropped, not ranked first"
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].entity_id, "finite");
    }

    #[test]
    fn unpack_rejects_nonfinite_components() {
        use temper_runtime::persistence::{pack_f32_le, unpack_f32_le};
        assert!(unpack_f32_le(&pack_f32_le(&[1.0, 2.0])).is_some());
        assert!(unpack_f32_le(&pack_f32_le(&[1.0, f32::NAN])).is_none());
        assert!(unpack_f32_le(&pack_f32_le(&[f32::INFINITY, 0.0])).is_none());
    }
}
