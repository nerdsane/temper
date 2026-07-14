//! Reconciliation of legacy feature-request projections into stable revisions.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use temper_evolution::FeatureRequestRecord;
use temper_store_turso::FeatureRequestRow;

fn normalized_trajectory_refs(raw: &str) -> Option<Vec<String>> {
    let mut refs = serde_json::from_str::<Vec<String>>(raw).ok()?;
    refs.sort();
    Some(refs)
}

fn evidence_description_prefix(description: &str) -> &str {
    description
        .split_once(" — ")
        .map_or(description, |(prefix, _)| prefix)
}

fn is_same_evidence_revision(
    row: &FeatureRequestRow,
    category: &str,
    feature_request: &FeatureRequestRecord,
) -> bool {
    let mut generated_refs = feature_request.trajectory_refs.clone();
    generated_refs.sort();
    row.category == category
        && row.frequency == feature_request.frequency as i64
        && evidence_description_prefix(&row.description)
            == evidence_description_prefix(&feature_request.description)
        && normalized_trajectory_refs(&row.trajectory_refs).as_ref() == Some(&generated_refs)
}

fn is_legacy_feature_request_id(id: &str) -> bool {
    let mut parts = id.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some("FR"), Some(year), Some(suffix), None)
            if year.len() == 4
                && year.chars().all(|character| character.is_ascii_digit())
                && suffix.len() == 12
                && suffix.chars().all(|character| character.is_ascii_hexdigit())
    )
}

fn merged_review_state(rows: &[&FeatureRequestRow]) -> (String, Option<String>) {
    let reviewed = rows
        .iter()
        .copied()
        .filter(|row| {
            row.disposition != "Open"
                || row
                    .developer_notes
                    .as_deref()
                    .is_some_and(|notes| !notes.trim().is_empty())
        })
        .max_by(|left, right| (&left.updated_at, &left.id).cmp(&(&right.updated_at, &right.id)));
    let disposition = reviewed
        .map(|row| row.disposition.clone())
        .unwrap_or_else(|| "Open".to_string());

    let mut notes = BTreeSet::new();
    for row in rows {
        if let Some(row_notes) = row.developer_notes.as_deref() {
            for note in row_notes.split("\n\n").map(str::trim) {
                if !note.is_empty() {
                    notes.insert(note.to_string());
                }
            }
        }
    }
    (
        disposition,
        (!notes.is_empty()).then(|| notes.into_iter().collect::<Vec<_>>().join("\n\n")),
    )
}

pub(super) async fn reconcile_legacy_feature_requests(
    store: &dyn crate::storage::MetadataStore,
    existing_rows: &[FeatureRequestRow],
    stable_id: &str,
    feature_request: &FeatureRequestRecord,
) -> Result<(), StatusCode> {
    let category = format!("{:?}", feature_request.category);
    let matching_rows = existing_rows
        .iter()
        .filter(|row| {
            (row.id == stable_id || is_legacy_feature_request_id(&row.id))
                && is_same_evidence_revision(row, &category, feature_request)
        })
        .collect::<Vec<_>>();
    let obsolete_ids = matching_rows
        .iter()
        .filter(|row| row.id != stable_id && is_legacy_feature_request_id(&row.id))
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    if obsolete_ids.is_empty() {
        return Ok(());
    }

    let (disposition, developer_notes) = merged_review_state(&matching_rows);
    let canonical_updated = store
        .update_feature_request(stable_id, &disposition, developer_notes.as_deref())
        .await
        .map_err(|error| {
            tracing::error!(error = %error, backend = store.backend_name(), feature_request_id = stable_id, "failed to preserve legacy feature-request review state");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if !canonical_updated {
        tracing::error!(
            backend = store.backend_name(),
            feature_request_id = stable_id,
            "canonical feature request disappeared during reconciliation"
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    for obsolete_id in &obsolete_ids {
        store
            .delete_feature_request(obsolete_id)
            .await
            .map_err(|error| {
                tracing::error!(error = %error, backend = store.backend_name(), feature_request_id = obsolete_id, "failed to remove reconciled legacy feature request");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }
    tracing::info!(
        feature_request_id = stable_id,
        reconciled_count = obsolete_ids.len(),
        "reconciled legacy feature-request projections"
    );
    Ok(())
}
