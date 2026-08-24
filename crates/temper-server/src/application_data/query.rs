//! Closed query AST evaluation over canonical entity state.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use temper_wasm_sdk::data::{
    CompareOperatorV1, DataOperationKind, DataResultV1, FilterV1, ManifestEntityV1,
    ModuleDataError, ModuleDataErrorKind, OrderDirectionV1, OrderV1, PageV1, ScalarV1,
    SequencedValueV1,
};

use crate::storage::QueryFieldIndexOrderTarget;
use crate::storage::{QueryFieldIndexOrder, QueryFieldIndexOrderDirection};

use super::{
    ApplicationDataInvocation, GovernedApplicationDataService, data_error, internal_error,
    short_type,
};

#[path = "query/decimal.rs"]
mod decimal;
use decimal::compare_decimal;
#[path = "query/order.rs"]
mod order;
use order::compare_fallback_entities;

impl ApplicationDataInvocation {
    pub(super) async fn entity_query(
        &self,
        entity_type: &str,
        filter: Option<&FilterV1>,
        order_by: &[OrderV1],
        page: &PageV1,
    ) -> Result<DataResultV1, ModuleDataError> {
        self.require(DataOperationKind::EntityQuery, entity_type, None)?;
        self.authorize("list", entity_type, None)?;
        if page.limit == 0 || page.limit > self.authority.binding.grant.budgets.max_page_items {
            return Err(data_error(
                ModuleDataErrorKind::BudgetExceeded,
                "PageBudgetExceeded",
                "query page limit exceeds the module grant",
            ));
        }
        let entity_grant = self
            .authority
            .binding
            .grant
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "CapabilityDenied",
                    "entity is not granted",
                )
            })?;
        let mut filter_nodes = 0_u32;
        let schema_entity = self
            .authority
            .binding
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "UnknownEntityType",
                    "entity type is absent from the bound schema",
                )
            })?;
        validate_filter_fields(
            filter,
            &entity_grant.query_filter_fields,
            schema_entity,
            0,
            &mut filter_nodes,
        )?;
        for order in order_by {
            match order {
                OrderV1::Property { field, .. }
                    if !entity_grant.query_order_fields.contains(field) =>
                {
                    return Err(data_error(
                        ModuleDataErrorKind::AuthorizationDenied,
                        "QueryFieldDenied",
                        "query ordering field is not granted",
                    ));
                }
                OrderV1::EntityCommitSequence { .. } if !entity_grant.query_order_by_sequence => {
                    return Err(data_error(
                        ModuleDataErrorKind::AuthorizationDenied,
                        "QuerySequenceDenied",
                        "query ordering by entity commit sequence is not granted",
                    ));
                }
                _ => {}
            }
        }
        let digest = query_digest(entity_type, filter, order_by)?;
        let start = parse_cursor(page.cursor.as_deref(), &digest)?;
        let scan_budget =
            (self.authority.binding.grant.budgets.max_page_items as usize).saturating_mul(8);
        let projected_order = order_by
            .iter()
            .map(|order| QueryFieldIndexOrder {
                target: match order {
                    OrderV1::Property { field, .. } => {
                        QueryFieldIndexOrderTarget::Property(field.clone())
                    }
                    OrderV1::EntityCommitSequence { .. } => {
                        QueryFieldIndexOrderTarget::EntityCommitSequence
                    }
                },
                direction: match order.direction() {
                    OrderDirectionV1::Asc => QueryFieldIndexOrderDirection::Asc,
                    OrderDirectionV1::Desc => QueryFieldIndexOrderDirection::Desc,
                },
            })
            .chain(std::iter::once(QueryFieldIndexOrder {
                target: QueryFieldIndexOrderTarget::EntityId,
                direction: QueryFieldIndexOrderDirection::Asc,
            }))
            .collect::<Vec<_>>();
        let service = GovernedApplicationDataService::new(&self.state);
        let mut fallback_responses = BTreeMap::new();
        let ids = match service
            .query_candidates(
                &self.authority.tenant,
                short_type(entity_type),
                &projected_order,
                start,
                scan_budget.saturating_add(1),
            )
            .await
            .map_err(internal_error)?
        {
            Some(ids) => {
                tracing::Span::current().record("consistency_path", "query_plane");
                ids
            }
            None => {
                tracing::Span::current().record("consistency_path", "authoritative_fallback");
                let mut ids = service
                    .bounded_fallback_candidates(
                        &self.authority.tenant,
                        short_type(entity_type),
                        scan_budget,
                    )
                    .map_err(|_| {
                        data_error(
                            ModuleDataErrorKind::ConsistencyUnavailable,
                            "BoundedQueryFallbackUnavailable",
                            "authoritative query fallback cannot be bounded",
                        )
                    })?;
                if ids.len() > scan_budget {
                    return Err(data_error(
                        ModuleDataErrorKind::BudgetExceeded,
                        "QueryFallbackBudgetExceeded",
                        "authoritative query fallback exceeds the bounded scan budget",
                    ));
                }
                for id in &ids {
                    let response = service
                        .get(&self.authority.tenant, short_type(entity_type), id)
                        .await
                        .map_err(internal_error)?;
                    fallback_responses.insert(id.clone(), response);
                }
                ids.sort_by(|left, right| {
                    compare_fallback_entities(
                        left,
                        fallback_responses.get(left),
                        right,
                        fallback_responses.get(right),
                        order_by,
                        schema_entity,
                    )
                });
                ids.into_iter().skip(start).collect()
            }
        };
        let has_unscanned_candidate = ids.len() > scan_budget;
        let mut values = Vec::new();
        let mut scanned = 0_usize;
        for id in ids.into_iter().take(scan_budget) {
            scanned = scanned.saturating_add(1);
            let response = if let Some(response) = fallback_responses.remove(&id) {
                response
            } else {
                service
                    .get(&self.authority.tenant, short_type(entity_type), &id)
                    .await
                    .map_err(internal_error)?
            };
            let object = response
                .state
                .fields
                .as_object()
                .cloned()
                .unwrap_or_default();
            if self
                .authorize_value("read", entity_type, Some(&id), Some(&object))
                .is_err()
            {
                continue;
            }
            if filter.is_none_or(|filter| matches_filter(filter, &object)) {
                values.push(SequencedValueV1 {
                    value: object,
                    sequence: response.state.sequence_nr,
                });
                if values.len() == page.limit as usize {
                    break;
                }
            }
        }
        let next_offset = start.saturating_add(scanned);
        let next_cursor = (has_unscanned_candidate || values.len() == page.limit as usize)
            .then(|| format!("v1:{digest}:{next_offset}"));
        Ok(DataResultV1::Page {
            values,
            next_cursor,
        })
    }
}

fn validate_filter_fields(
    filter: Option<&FilterV1>,
    granted: &std::collections::BTreeSet<String>,
    schema_entity: &ManifestEntityV1,
    depth: u32,
    nodes: &mut u32,
) -> Result<(), ModuleDataError> {
    let Some(filter) = filter else { return Ok(()) };
    *nodes = nodes.saturating_add(1);
    if depth > 8 || *nodes > 64 {
        return Err(data_error(
            ModuleDataErrorKind::BudgetExceeded,
            "FilterBudgetExceeded",
            "query filter depth or node budget exceeded",
        ));
    }
    match filter {
        FilterV1::Compare {
            field,
            operator,
            value,
        } => {
            if !granted.contains(field) {
                return Err(data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "QueryFieldDenied",
                    "query filter field is not granted",
                ));
            }
            let property = schema_entity
                .properties
                .iter()
                .find(|property| property.canonical_name == *field)
                .ok_or_else(|| {
                    data_error(
                        ModuleDataErrorKind::SchemaMismatch,
                        "UnknownQueryProperty",
                        "query property is absent from the bound schema",
                    )
                })?;
            if !scalar_matches_type(value, property)
                || matches!(
                    operator,
                    CompareOperatorV1::Lt
                        | CompareOperatorV1::Le
                        | CompareOperatorV1::Gt
                        | CompareOperatorV1::Ge
                ) && property.type_name == "Edm.Boolean"
            {
                return Err(data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "QueryTypeMismatch",
                    "query scalar or operator is incompatible with the property type",
                ));
            }
        }
        FilterV1::IsNull { field, .. } => {
            if !granted.contains(field) {
                return Err(data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "QueryFieldDenied",
                    "query filter field is not granted",
                ));
            }
            let property = schema_entity
                .properties
                .iter()
                .find(|property| property.canonical_name == *field)
                .ok_or_else(|| {
                    data_error(
                        ModuleDataErrorKind::SchemaMismatch,
                        "UnknownQueryProperty",
                        "query property is absent from the bound schema",
                    )
                })?;
            if !property.nullable {
                return Err(data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "NonNullableQueryProperty",
                    "is_null is not valid for a non-nullable property",
                ));
            }
        }
        FilterV1::And { operands } | FilterV1::Or { operands } => {
            if operands.is_empty() {
                return Err(data_error(
                    ModuleDataErrorKind::InvalidRequest,
                    "EmptyFilter",
                    "and/or filters require at least one operand",
                ));
            }
            for operand in operands {
                validate_filter_fields(Some(operand), granted, schema_entity, depth + 1, nodes)?;
            }
        }
        FilterV1::Not { operand } => {
            validate_filter_fields(Some(operand), granted, schema_entity, depth + 1, nodes)?
        }
    }
    Ok(())
}

fn scalar_matches_type(
    value: &ScalarV1,
    property: &temper_wasm_sdk::data::ManifestPropertyV1,
) -> bool {
    let type_name = property.type_name.as_str();
    match value {
        ScalarV1::Boolean(_) => type_name == "Edm.Boolean",
        ScalarV1::Int64(_) => matches!(
            type_name,
            "Edm.Byte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64"
        ),
        ScalarV1::Double(_) => matches!(type_name, "Edm.Single" | "Edm.Double"),
        ScalarV1::Guid(_) => type_name == "Edm.Guid",
        ScalarV1::DateTimeOffset(_) => type_name == "Edm.DateTimeOffset",
        ScalarV1::Decimal(_) => type_name == "Edm.Decimal",
        ScalarV1::Enum(value) => {
            value.type_name == type_name && property.enum_members.contains(&value.member)
        }
        ScalarV1::String(_) => type_name == "Edm.String",
    }
}

fn query_digest(
    entity_type: &str,
    filter: Option<&FilterV1>,
    order_by: &[OrderV1],
) -> Result<String, ModuleDataError> {
    let canonical = serde_json::to_vec(&(entity_type, filter, order_by)).map_err(|error| {
        data_error(
            ModuleDataErrorKind::InvalidRequest,
            "InvalidQuery",
            &error.to_string(),
        )
    })?;
    Ok(format!("{:x}", Sha256::digest(canonical)))
}

fn parse_cursor(cursor: Option<&str>, digest: &str) -> Result<usize, ModuleDataError> {
    let Some(cursor) = cursor else { return Ok(0) };
    let mut parts = cursor.split(':');
    if parts.next() != Some("v1") || parts.next() != Some(digest) {
        return Err(data_error(
            ModuleDataErrorKind::InvalidRequest,
            "CursorQueryMismatch",
            "cursor does not belong to this query",
        ));
    }
    let offset = parts.next().and_then(|value| value.parse::<usize>().ok());
    if parts.next().is_some() || offset.is_none() {
        return Err(data_error(
            ModuleDataErrorKind::InvalidRequest,
            "InvalidCursor",
            "query cursor is invalid",
        ));
    }
    Ok(offset.unwrap_or_default())
}

fn matches_filter(filter: &FilterV1, value: &Map<String, Value>) -> bool {
    match filter {
        FilterV1::Compare {
            field,
            operator,
            value: expected,
        } => compare_scalar(value.get(field), expected).is_some_and(|ordering| match operator {
            CompareOperatorV1::Eq => ordering == Ordering::Equal,
            CompareOperatorV1::Ne => ordering != Ordering::Equal,
            CompareOperatorV1::Lt => ordering == Ordering::Less,
            CompareOperatorV1::Le => ordering != Ordering::Greater,
            CompareOperatorV1::Gt => ordering == Ordering::Greater,
            CompareOperatorV1::Ge => ordering != Ordering::Less,
        }),
        FilterV1::IsNull { field, is_null } => {
            value.get(field).is_none_or(Value::is_null) == *is_null
        }
        FilterV1::And { operands } => operands
            .iter()
            .all(|operand| matches_filter(operand, value)),
        FilterV1::Or { operands } => operands
            .iter()
            .any(|operand| matches_filter(operand, value)),
        FilterV1::Not { operand } => !matches_filter(operand, value),
    }
}

fn compare_scalar(actual: Option<&Value>, expected: &ScalarV1) -> Option<Ordering> {
    match (actual?, expected) {
        (Value::Bool(actual), ScalarV1::Boolean(expected)) => actual.partial_cmp(expected),
        (Value::Number(actual), ScalarV1::Int64(expected)) => {
            actual.as_i64()?.partial_cmp(expected)
        }
        (Value::Number(actual), ScalarV1::Double(expected)) => {
            actual.as_f64()?.partial_cmp(expected)
        }
        (Value::String(actual), ScalarV1::String(expected) | ScalarV1::Guid(expected)) => {
            actual.partial_cmp(expected)
        }
        (Value::String(actual), ScalarV1::DateTimeOffset(expected)) => {
            let actual = chrono::DateTime::parse_from_rfc3339(actual).ok()?;
            let expected = chrono::DateTime::parse_from_rfc3339(expected).ok()?;
            actual.partial_cmp(&expected)
        }
        (Value::String(actual), ScalarV1::Decimal(expected)) => compare_decimal(actual, expected),
        (Value::String(actual), ScalarV1::Enum(expected)) => actual.partial_cmp(&expected.member),
        _ => None,
    }
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
