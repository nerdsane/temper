//! `Temper.Nearest` — the exact-scan kNN bound function (ADR-0155).
//!
//! A collection-bound OData function that ranks a declared `[[vector]]` path:
//!
//! ```text
//! GET /tdata/<Set>/Temper.Nearest(decl='taste',to='<id>',k=10)
//! GET /tdata/<Set>/Temper.Nearest(decl='taste',vector='[…]',k=10,model='<tag>')
//! GET /tdata/<Set>/Temper.Nearest(decl='taste',to='<id>',k=10,filter='Status eq ''Published''')
//! ```
//!
//! The store supplies the partition's candidate `(entity_id, vector)` rows in a
//! fixed order; [`crate::vector_index`] ranks them (nearest first, id tiebreak). The
//! read runs under the same Cedar `list`/`read` gates and the same candidate budget
//! as any query-plane read — that is the whole point of doing kNN in the kernel.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::path::ODataPath;
use temper_odata::query::types::QueryOptions;
use temper_runtime::tenant::TenantId;

use super::authz::{LIST_ACTION, READ_ACTION, authorize_read};
use super::common::resolve_entity_type;
use super::query_plane_read::QueryPlaneReadBudget;
use super::read_support::materialize_entity_set_entities;
use crate::response::odata_error;
use crate::state::ServerState;
use crate::vector_index::{VectorMetric, parse_vector_property, rank_nearest};

/// Look up a named argument in the parsed bound-function parameter list.
fn arg<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Read a property off a materialized entity body. The temper OData body nests state
/// properties under a `"fields"` object (`body["fields"]["Embedding"]`); some catalog
/// shapes also flatten them to the top level. Check `fields` first, then top level,
/// each tolerant of the snake/Pascal field-name split the rest of the read plane
/// accommodates.
fn entity_field<'a>(body: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    let snake = temper_spec::to_snake_case(name);
    let pascal = temper_spec::to_pascal_case(name);
    if let Some(fields) = body.get("fields").and_then(|f| f.as_object())
        && let Some(value) = fields
            .get(name)
            .or_else(|| fields.get(&snake))
            .or_else(|| fields.get(&pascal))
    {
        return Some(value);
    }
    let obj = body.as_object()?;
    obj.get(name)
        .or_else(|| obj.get(&snake))
        .or_else(|| obj.get(&pascal))
}

fn bad_request(message: &str) -> Response {
    odata_error(StatusCode::BAD_REQUEST, "InvalidNearestQuery", message).into_response()
}

/// Handle `GET /<Set>/Temper.Nearest(...)`.
pub(super) async fn handle_nearest(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    params: &[(String, String)],
    _query_options: &QueryOptions,
) -> Response {
    // Temper.Nearest is collection-bound: the parent must be an entity set.
    let set_name = match parent {
        ODataPath::EntitySet(name) => name.clone(),
        _ => {
            return bad_request(
                "Temper.Nearest is bound to an entity collection, e.g. /DesignLanguages/Temper.Nearest(...)",
            );
        }
    };
    let entity_type = match resolve_entity_type(state, tenant, &set_name) {
        Some(et) => et,
        None => {
            return odata_error(
                StatusCode::NOT_FOUND,
                "ResourceNotFound",
                &format!("Entity set '{set_name}' not found"),
            )
            .into_response();
        }
    };

    // Resolve the declared vector path named by `decl=`.
    let Some(decl_name) = arg(params, "decl") else {
        return bad_request("Temper.Nearest requires a 'decl' argument naming the vector path");
    };
    let vectors = state.declared_vectors_for(tenant, &entity_type);
    let Some(decl) = vectors.iter().find(|v| v.name == decl_name) else {
        return bad_request(&format!(
            "entity type '{entity_type}' declares no vector path named '{decl_name}'"
        ));
    };
    let Some(metric) = VectorMetric::parse(&decl.metric) else {
        return odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InvalidVectorMetric",
            &format!("vector path '{decl_name}' has an unsupported metric"),
        )
        .into_response();
    };

    // Cedar: the collection-level list gate, up front (same gate as an ordinary
    // list read). Per-row read is enforced during materialization below.
    if let Err(response) = authorize_read(
        state,
        tenant,
        security_ctx,
        LIST_ACTION,
        &entity_type,
        "",
        &serde_json::json!({}),
    ) {
        return *response;
    }

    let budget = QueryPlaneReadBudget::from_config();
    let k = match arg(params, "k") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(k) if k >= 1 => k.min(budget.max_result_k()),
            _ => return bad_request("'k' must be a positive integer"),
        },
        None => 10usize.min(budget.max_result_k()).max(1),
    };

    // Resolve the query vector + its model tag from either `to=` (another entity)
    // or `vector=` (a raw query vector). Exactly one is required.
    let to_arg = arg(params, "to");
    let vector_arg = arg(params, "vector");
    let model_arg = arg(params, "model");
    let (query_vector, model_tag, exclude_id): (Vec<f32>, String, Option<String>) = match (
        to_arg, vector_arg,
    ) {
        (Some(_), Some(_)) => {
            return bad_request("Temper.Nearest takes exactly one of 'to' or 'vector', not both");
        }
        (None, None) => {
            return bad_request("Temper.Nearest requires either 'to' (an entity id) or 'vector'");
        }
        (Some(reference_id), None) => {
            let materialized = materialize_entity_set_entities(
                state,
                tenant,
                &entity_type,
                &set_name,
                std::slice::from_ref(&reference_id.to_string()),
                true,
                None,
            )
            .await;
            let Some(body) = materialized.entities.into_iter().next() else {
                return odata_error(
                    StatusCode::NOT_FOUND,
                    "ResourceNotFound",
                    &format!("reference entity '{reference_id}' not found"),
                )
                .into_response();
            };
            let Some(vector) = entity_field(&body, &decl.property)
                .and_then(|v| parse_vector_property(v, decl.dims))
            else {
                return bad_request(&format!(
                    "reference entity '{reference_id}' has no usable '{}' vector for path '{decl_name}'",
                    decl.property
                ));
            };
            let model_tag = match model_arg {
                Some(tag) => tag.to_string(),
                None => match entity_field(&body, &decl.model_property)
                    .and_then(|v| v.as_str())
                    .filter(|tag| !tag.is_empty())
                {
                    Some(tag) => tag.to_string(),
                    None => {
                        return bad_request(&format!(
                            "reference entity '{reference_id}' has no '{}' model tag; pass 'model' explicitly",
                            decl.model_property
                        ));
                    }
                },
            };
            (vector, model_tag, Some(reference_id.to_string()))
        }
        (None, Some(raw_vector)) => {
            let Some(vector) = parse_vector_property(
                &serde_json::Value::String(raw_vector.to_string()),
                decl.dims,
            ) else {
                return bad_request(&format!(
                    "'vector' must be a JSON array of exactly {} finite numbers",
                    decl.dims
                ));
            };
            let Some(model_tag) = model_arg.filter(|tag| !tag.is_empty()) else {
                return bad_request("'model' is required when querying by raw 'vector'");
            };
            (vector, model_tag.to_string(), None)
        }
    };

    // Optional pre-ranking equality filter (`filter='Status eq ''Published'''`).
    let equality_filter = match arg(params, "filter") {
        Some(raw) => match temper_odata::query::filter::parse_filter(raw) {
            Ok(expr) => match super::filter_sql::equality_field_predicates(&expr) {
                Some(pairs) => Some(pairs),
                None => {
                    return bad_request(
                        "Temper.Nearest 'filter' supports only equality predicates joined by 'and'",
                    );
                }
            },
            Err(error) => return bad_request(&format!("invalid 'filter': {error}")),
        },
        None => None,
    };

    // Candidate scan: pull the partition's vectors and charge the read budget.
    let Some((store, _)) = state.event_journal() else {
        return odata_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "StorageUnavailable",
            "vector search requires a query-plane store",
        )
        .into_response();
    };
    let candidates = match store
        .vector_candidates(tenant.as_str(), &entity_type, decl_name, &model_tag)
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "VectorScanFailed",
                &format!("vector candidate scan failed: {error}"),
            )
            .into_response();
        }
    };
    if candidates.len() > budget.candidate_budget() {
        return odata_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "QueryTooLarge",
            "This vector partition holds more candidates than the bounded read budget permits. Declare an ANN index or narrow the model partition.",
        )
        .into_response();
    }

    // Rank every candidate; walk in nearest-first order, materializing +
    // equality-filtering + read-authorizing lazily until k rows are accepted.
    // Walking the full ranked list and taking the first k that pass the filter is
    // exactly "filter, then take the top k".
    let ranked = rank_nearest(
        metric,
        &query_vector,
        &candidates,
        candidates.len(),
        exclude_id.as_deref(),
    );

    let mut value: Vec<serde_json::Value> = Vec::with_capacity(k);
    for scored in ranked {
        if value.len() >= k {
            break;
        }
        let materialized = materialize_entity_set_entities(
            state,
            tenant,
            &entity_type,
            &set_name,
            std::slice::from_ref(&scored.entity_id),
            true,
            None,
        )
        .await;
        let Some(mut body) = materialized.entities.into_iter().next() else {
            continue;
        };
        if let Some(pairs) = &equality_filter
            && !body_matches_equality(&body, pairs)
        {
            continue;
        }
        if authorize_read(
            state,
            tenant,
            security_ctx,
            READ_ACTION,
            &entity_type,
            &scored.entity_id,
            &body,
        )
        .is_err()
        {
            continue;
        }
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "@temper.score".to_string(),
                serde_json::Number::from_f64(scored.score as f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        value.push(body);
    }

    crate::response::ODataResponse {
        status: StatusCode::OK,
        body: serde_json::json!({
            "@odata.context": format!("$metadata#{set_name}"),
            "value": value,
        }),
    }
    .into_response()
}

/// Whether a materialized entity body satisfies every equality predicate. A
/// missing field matches only `field eq null`. Values compare structurally (the
/// filter literals are already JSON). Field lookup is `fields`-nesting aware and
/// tolerant of the snake/Pascal split, via [`entity_field`].
fn body_matches_equality(body: &serde_json::Value, pairs: &[(String, serde_json::Value)]) -> bool {
    pairs
        .iter()
        .all(|(field, expected)| match entity_field(body, field) {
            Some(value) => value == expected,
            None => expected.is_null(),
        })
}
