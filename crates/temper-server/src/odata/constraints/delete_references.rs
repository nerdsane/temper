//! Finding the rows that reference an entity being deleted: the rows of a
//! relation's referencing type whose foreign-key field holds the entity's id.
//!
//! The query projection's field index answers that the way it answers
//! `$filter=<field> eq '<id>'`: one indexed probe, with no referencing entity
//! read from its actor. A type with thousands of rows costs the same as one
//! with none.
//!
//! The index is written after the journal (ADR-0148), and a committed change
//! stays committed when its projection fails, so the index can only vouch for
//! rows whose catalog entry is current: no journal event newer than the one it
//! was projected from. Before a type is first probed, the finder reads which of
//! its catalog rows are current. A probe hit counts only on a row that is not
//! stale, and the live rows the index cannot vouch for (never projected, or
//! projected before a later change) are read from their actors. When the index
//! cannot answer at all (no query plane, a field declared
//! `query_indexed = false`, an id too long to index, a failed read), every live
//! row of the type is in that set. The set is read once per type per delete,
//! and only while it fits [`UNINDEXED_REFERENCE_READ_BUDGET`]; past it the
//! lookup reports that it could not check the rows instead of reading the
//! whole type.
//!
//! A row holds a reference where the create check reads it (`extract_field`):
//! the field at the top level, else inside a `fields` object, which is how a
//! write that wraps its body under `fields` stores it. The index holds only
//! top-level fields, so the catalog rows that hold a `fields` object come back
//! with their projected fields, and a current one is checked from those.

use std::collections::{BTreeMap, BTreeSet};

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::tenant::TenantId;

use crate::registry::RelationEdge;
use crate::state::ServerState;

/// Most referencing rows one delete reads from their actors, per type.
///
/// Reading a row the index cannot vouch for can restore its actor from
/// storage, and production restores about 40 actors a second, so this keeps a
/// delete to a few seconds of reads. Projection lag or a failed projection
/// leaves a handful of rows unvouched; a larger set means the type has no
/// usable projection, and reading all of it is the request that ran for
/// minutes before this lookup.
const UNINDEXED_REFERENCE_READ_BUDGET: usize = 100;

/// Which live rows of a type the field index cannot vouch for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unvouched {
    /// The index answered: rows whose catalog entry is not current.
    NotCurrent,
    /// The index could not answer: every live row.
    Every,
}

/// A type's projection catalog as the finder read it, before its first probe.
///
/// A row that is current when this is read holds, by the time of a later
/// probe, either those fields or ones a later commit projected; a change
/// committed after this read races the delete as any concurrent write does. A
/// row projected after this read is in neither set: a hit on it is a committed
/// state that held the id, and it is read from its actor when live.
struct Catalog {
    /// Rows whose journal holds no event newer than the one projected.
    current: BTreeSet<String>,
    /// Rows whose journal has moved past them. Their index entries may hold
    /// earlier fields, so a probe hit on one proves nothing.
    stale: BTreeSet<String>,
    /// Current rows that hold a `fields` object, with their projected
    /// fields: the index does not look inside that object.
    wrapped: Vec<(String, serde_json::Value)>,
}

/// Rows read from their actors because the index could not vouch for them.
type ReadRows = Vec<(String, serde_json::Value)>;

/// Answers "which rows reference this id" for every relation one delete
/// looks at, reading each type's catalog and unvouched rows at most once.
pub(super) struct ReferenceFinder<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    /// Per type; `None` when the store cannot say which rows are current.
    catalogs: BTreeMap<String, Option<Catalog>>,
    read_rows: BTreeMap<(String, Unvouched), Result<ReadRows, String>>,
}

impl<'a> ReferenceFinder<'a> {
    pub(super) fn new(state: &'a ServerState, tenant: &'a TenantId) -> Self {
        Self {
            state,
            tenant,
            catalogs: BTreeMap::new(),
            read_rows: BTreeMap::new(),
        }
    }

    /// Ids of `edge.from_entity` rows whose `edge.source_field` holds
    /// `entity_id`, leaving out the ids in `skip`, at most `limit` of them.
    /// `Err` says which rows could not be checked and why.
    pub(super) async fn referencing_ids(
        &mut self,
        edge: &RelationEdge,
        entity_id: &str,
        skip: &BTreeSet<String>,
        limit: usize,
    ) -> Result<Vec<String>, String> {
        debug_assert!(!edge.from_entity.is_empty());
        debug_assert!(!edge.source_field.is_empty());
        self.load_catalog(&edge.from_entity).await;
        let probed = match self.catalogs[&edge.from_entity].as_ref() {
            Some(catalog) => {
                self.probe_field_index(edge, entity_id, skip, limit, catalog)
                    .await
            }
            None => None,
        };
        let (mut found, unvouched) = match probed {
            Some(ids) => (ids, Unvouched::NotCurrent),
            None => (Vec::new(), Unvouched::Every),
        };
        if found.len() >= limit {
            found.truncate(limit);
            return Ok(found);
        }

        let rows = self.unvouched_rows(&edge.from_entity, unvouched).await?;
        for (source_id, fields) in rows {
            if found.len() >= limit {
                break;
            }
            if holds_id(fields, &edge.source_field, entity_id)
                && !skip.contains(source_id)
                && !found.contains(source_id)
            {
                found.push(source_id.clone());
            }
        }
        Ok(found)
    }

    /// Read which catalog rows of `entity_type` are current, once per delete.
    async fn load_catalog(&mut self, entity_type: &str) {
        if self.catalogs.contains_key(entity_type) {
            return;
        }
        let catalog = match self.state.query_plane_store() {
            Some(query_plane) => match query_plane
                .projection_catalog_freshness(self.tenant.as_str(), entity_type)
                .await
            {
                Ok(rows) => rows.map(|rows| {
                    let mut catalog = Catalog {
                        current: BTreeSet::new(),
                        stale: BTreeSet::new(),
                        wrapped: Vec::new(),
                    };
                    for row in rows {
                        if !row.current {
                            catalog.stale.insert(row.entity_id);
                            continue;
                        }
                        if let Some(fields) = row.wrapped_fields {
                            catalog.wrapped.push((row.entity_id.clone(), fields));
                        }
                        catalog.current.insert(row.entity_id);
                    }
                    catalog
                }),
                Err(error) => {
                    tracing::warn!(
                        tenant = %self.tenant, entity_type, error = %error,
                        "projection catalog freshness read failed; treating every live row as unindexed"
                    );
                    None
                }
            },
            None => None,
        };
        self.catalogs.insert(entity_type.to_string(), catalog);
    }

    /// Ask the field index for up to `limit` rows outside `skip` and the
    /// catalog's stale rows whose `source_field` is `entity_id`, using the
    /// same predicate an OData `$filter` equality pushes down, and add the
    /// current rows that hold it inside a `fields` object. `None` when the
    /// index cannot answer.
    async fn probe_field_index(
        &self,
        edge: &RelationEdge,
        entity_id: &str,
        skip: &BTreeSet<String>,
        limit: usize,
        catalog: &Catalog,
    ) -> Option<Vec<String>> {
        let query_plane = self.state.query_plane_store()?;
        if !self.state.query_projection_indexes_field(
            self.tenant,
            &edge.from_entity,
            &edge.source_field,
        ) {
            return None;
        }
        let equality = FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property(edge.source_field.clone())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                entity_id.to_string(),
            ))),
        };
        // `None` when the id is longer than the index stores a value.
        let predicate = crate::odata::filter_sql::try_translate_candidate_filter(&equality)?;
        // Skipped and stale rows can fill the page; asking for that many more
        // than `limit` reaches `limit` others whenever that many match.
        let top = limit
            .saturating_add(skip.len())
            .saturating_add(catalog.stale.len());
        match query_plane
            .query_field_index_page(
                self.tenant.as_str(),
                &edge.from_entity,
                &predicate.where_clause,
                predicate.params,
                &[],
                0,
                top,
                false,
            )
            .await
        {
            Ok(Some(page)) => {
                debug_assert!(page.entity_ids.len() <= top);
                let mut found: Vec<String> = page
                    .entity_ids
                    .into_iter()
                    .filter(|id| !skip.contains(id) && !catalog.stale.contains(id))
                    .collect();
                for (source_id, fields) in &catalog.wrapped {
                    if holds_id(fields, &edge.source_field, entity_id)
                        && !skip.contains(source_id)
                        && !found.contains(source_id)
                    {
                        found.push(source_id.clone());
                    }
                }
                Some(found)
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    tenant = %self.tenant, from_entity = %edge.from_entity, error = %error,
                    "delete reference probe failed; checking unindexed rows instead"
                );
                None
            }
        }
    }

    /// The rows of `entity_type` the index cannot vouch for, read from their
    /// actors once per delete while they fit the budget.
    async fn unvouched_rows(
        &mut self,
        entity_type: &str,
        unvouched: Unvouched,
    ) -> Result<&ReadRows, String> {
        let key = (entity_type.to_string(), unvouched);
        if !self.read_rows.contains_key(&key) {
            let live_ids = self
                .state
                .list_entity_ids_lazy(self.tenant, entity_type)
                .await;
            let ids = match (unvouched, self.catalogs.get(entity_type)) {
                (Unvouched::NotCurrent, Some(Some(catalog))) => live_ids
                    .into_iter()
                    .filter(|id| !catalog.current.contains(id))
                    .collect(),
                _ => live_ids,
            };
            let rows = self.read_within_budget(entity_type, &ids).await;
            self.read_rows.insert(key.clone(), rows);
        }
        match &self.read_rows[&key] {
            Ok(rows) => Ok(rows),
            Err(reason) => Err(reason.clone()),
        }
    }

    /// Read the rows the index could not vouch for, while they fit the budget.
    async fn read_within_budget(
        &self,
        entity_type: &str,
        ids: &[String],
    ) -> Result<ReadRows, String> {
        tracing::debug!(
            entity_type,
            unvouched = ids.len(),
            "delete reference rows to read"
        );
        if ids.len() > UNINDEXED_REFERENCE_READ_BUDGET {
            return Err(format!(
                "{} {} rows are not current in the query index, more than the {} a delete \
                 reads one by one; retry once the index has caught up",
                ids.len(),
                entity_type,
                UNINDEXED_REFERENCE_READ_BUDGET
            ));
        }
        debug_assert!(ids.len() <= UNINDEXED_REFERENCE_READ_BUDGET);

        let mut rows = Vec::with_capacity(ids.len());
        for source_id in ids {
            match self
                .state
                .get_tenant_entity_state(self.tenant, entity_type, source_id)
                .await
            {
                Ok(response) => rows.push((source_id.clone(), response.state.fields)),
                Err(error) => {
                    return Err(format!(
                        "{entity_type}('{source_id}') is not current in the query index and \
                         could not be read: {error}"
                    ));
                }
            }
        }
        Ok(rows)
    }
}

/// Whether the reference `fields` holds through `field` is `entity_id`, read
/// where the create check reads it (`extract_field`) and compared as the field
/// index stores a scalar: its text.
pub(super) fn holds_id(fields: &serde_json::Value, field: &str, entity_id: &str) -> bool {
    match super::extract_field(fields, field) {
        Some(serde_json::Value::String(value)) => value == entity_id,
        Some(serde_json::Value::Number(value)) => value.to_string() == entity_id,
        Some(serde_json::Value::Bool(value)) => value.to_string() == entity_id,
        _ => false,
    }
}
