use std::cmp::Ordering;

use serde_json::Value;
use temper_wasm_sdk::data::{ManifestEntityV1, OrderDirectionV1, OrderV1};

use super::compare_decimal;

pub(super) fn compare_fallback_entities(
    left_id: &str,
    left: Option<&crate::entity_actor::EntityResponse>,
    right_id: &str,
    right: Option<&crate::entity_actor::EntityResponse>,
    order_by: &[OrderV1],
    schema: &ManifestEntityV1,
) -> Ordering {
    let left_fields = left.and_then(|response| response.state.fields.as_object());
    let right_fields = right.and_then(|response| response.state.fields.as_object());
    for order in order_by {
        let ordering = match order {
            OrderV1::Property { field, direction } => {
                let type_name = schema
                    .properties
                    .iter()
                    .find(|property| property.canonical_name == *field)
                    .map(|property| property.type_name.as_str())
                    .unwrap_or("Edm.String");
                compare_order_values(
                    left_fields.and_then(|fields| fields.get(field)),
                    right_fields.and_then(|fields| fields.get(field)),
                    type_name,
                    *direction,
                )
            }
            OrderV1::EntityCommitSequence { direction } => compare_direction(
                left.map(|response| response.state.sequence_nr),
                right.map(|response| response.state.sequence_nr),
                *direction,
            ),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_id.cmp(right_id)
}

fn compare_direction<T: Ord>(
    left: Option<T>,
    right: Option<T>,
    direction: OrderDirectionV1,
) -> Ordering {
    let ordering = left.cmp(&right);
    match direction {
        OrderDirectionV1::Asc => ordering,
        OrderDirectionV1::Desc => ordering.reverse(),
    }
}

fn compare_order_values(
    left: Option<&Value>,
    right: Option<&Value>,
    type_name: &str,
    direction: OrderDirectionV1,
) -> Ordering {
    let left = left.filter(|value| !value.is_null());
    let right = right.filter(|value| !value.is_null());
    let ordering = match (left, right) {
        (None, None) => Ordering::Equal,
        // Query-plane parity: NULLS LAST for ascending and NULLS FIRST for descending.
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => compare_typed_json(left, right, type_name),
    };
    match direction {
        OrderDirectionV1::Asc => ordering,
        OrderDirectionV1::Desc => ordering.reverse(),
    }
}

fn compare_typed_json(left: &Value, right: &Value, type_name: &str) -> Ordering {
    match type_name {
        "Edm.Boolean" => left.as_bool().cmp(&right.as_bool()),
        "Edm.Byte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64" => left.as_i64().cmp(&right.as_i64()),
        "Edm.Single" | "Edm.Double" => left
            .as_f64()
            .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
            .unwrap_or(Ordering::Equal),
        "Edm.Decimal" => left
            .as_str()
            .zip(right.as_str())
            .and_then(|(left, right)| compare_decimal(left, right))
            .unwrap_or(Ordering::Equal),
        "Edm.DateTimeOffset" => left
            .as_str()
            .and_then(|left| chrono::DateTime::parse_from_rfc3339(left).ok())
            .zip(
                right
                    .as_str()
                    .and_then(|right| chrono::DateTime::parse_from_rfc3339(right).ok()),
            )
            .map(|(left, right)| left.cmp(&right))
            .unwrap_or(Ordering::Equal),
        _ => left.as_str().cmp(&right.as_str()),
    }
}
