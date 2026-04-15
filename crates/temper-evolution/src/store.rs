//! In-memory record store for evolution records.
//!
//! **Deprecated**: Evolution records are now managed as IOA entities in the
//! `temper-system` tenant (ADR-0025). This store is retained for backward
//! compatibility during migration. New code should use entity dispatch to
//! Observation, Problem, Analysis, EvolutionDecision, Insight, and
//! FeatureRequest entities instead.
//!
//! Production deployments would back this with Git + Postgres.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::Serialize;

use crate::records::*;

const RECORD_MAP_BUDGET: usize = 10_000;

/// Save a map of serializable records to a sub-directory as individual JSON files.
fn save_records_to_subdir<T: Serialize>(
    dir: &Path,
    sub_name: &str,
    records: &BTreeMap<RecordId, T>,
) -> std::io::Result<()> {
    let sub_dir = dir.join(sub_name);
    fs::create_dir_all(&sub_dir)?;
    for (id, record) in records {
        let path = sub_dir.join(format!("{id}.json"));
        let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
        fs::write(&path, json)?;
    }
    Ok(())
}

fn evict_oldest_if_over_budget<T>(
    records: &mut BTreeMap<RecordId, T>,
    timestamp_of: impl Fn(&T) -> i64,
) {
    while records.len() > RECORD_MAP_BUDGET {
        let oldest_id = records
            .iter()
            .min_by_key(|(id, record)| (timestamp_of(record), *id))
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest_id {
            records.remove(&id);
        } else {
            break;
        }
    }
}

/// Generate insert and get methods for a record type stored in a named field.
macro_rules! record_accessors {
    ($field:ident, $record_ty:ty, $insert_fn:ident, $insert_doc:literal, $get_fn:ident, $get_doc:literal) => {
        #[doc = $insert_doc]
        pub fn $insert_fn(&self, record: $record_ty) {
            let mut inner = self.inner.write().unwrap(); // ci-ok: infallible lock
            inner.$field.insert(record.header.id.clone(), record);
            evict_oldest_if_over_budget(&mut inner.$field, |r: &$record_ty| {
                r.header.timestamp.timestamp_millis()
            });
        }

        #[doc = $get_doc]
        pub fn $get_fn(&self, id: &str) -> Option<$record_ty> {
            self.inner.read().unwrap().$field.get(id).cloned() // ci-ok: infallible lock
        }
    };
}

/// Stores evolution records. In-memory implementation for now.
/// Production: Git (source of truth) + Postgres (indexed for querying).
///
/// **Deprecated**: Use IOA entity dispatch to the `temper-system` tenant instead (ADR-0025).
#[deprecated(note = "Use IOA entity dispatch to temper-system tenant (ADR-0025)")]
#[derive(Clone)]
pub struct RecordStore {
    inner: Arc<RwLock<StoreInner>>,
}

struct StoreInner {
    observations: BTreeMap<RecordId, ObservationRecord>,
    problems: BTreeMap<RecordId, ProblemRecord>,
    analyses: BTreeMap<RecordId, AnalysisRecord>,
    decisions: BTreeMap<RecordId, DecisionRecord>,
    insights: BTreeMap<RecordId, InsightRecord>,
}

#[allow(deprecated)]
impl RecordStore {
    /// Create a new, empty record store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StoreInner {
                observations: BTreeMap::new(),
                problems: BTreeMap::new(),
                analyses: BTreeMap::new(),
                decisions: BTreeMap::new(),
                insights: BTreeMap::new(),
            })),
        }
    }

    record_accessors!(
        observations,
        ObservationRecord,
        insert_observation,
        "Insert an observation record into the store.",
        get_observation,
        "Retrieve an observation record by ID."
    );
    record_accessors!(
        problems,
        ProblemRecord,
        insert_problem,
        "Insert a problem record into the store.",
        get_problem,
        "Retrieve a problem record by ID."
    );
    record_accessors!(
        analyses,
        AnalysisRecord,
        insert_analysis,
        "Insert an analysis record into the store.",
        get_analysis,
        "Retrieve an analysis record by ID."
    );
    record_accessors!(
        decisions,
        DecisionRecord,
        insert_decision,
        "Insert a decision record into the store.",
        get_decision,
        "Retrieve a decision record by ID."
    );
    record_accessors!(
        insights,
        InsightRecord,
        insert_insight,
        "Insert an insight record into the store.",
        get_insight,
        "Retrieve an insight record by ID."
    );

    /// Get all open observations (not yet resolved).
    pub fn open_observations(&self) -> Vec<ObservationRecord> {
        self.inner
            .read()
            .unwrap() // ci-ok: infallible lock
            .observations
            .values()
            .filter(|r| r.header.status == RecordStatus::Open)
            .cloned()
            .collect()
    }

    /// Get all insights sorted by priority score (highest first).
    pub fn ranked_insights(&self) -> Vec<InsightRecord> {
        let mut insights: Vec<InsightRecord> = self
            .inner
            .read()
            .unwrap() // ci-ok: infallible lock
            .insights
            .values()
            .filter(|r| r.header.status == RecordStatus::Open)
            .cloned()
            .collect();
        insights.sort_by(|a, b| {
            b.priority_score
                .partial_cmp(&a.priority_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        insights
    }

    /// Count records by type.
    pub fn count(&self, record_type: RecordType) -> usize {
        let inner = self.inner.read().unwrap();
        match record_type {
            RecordType::Observation => inner.observations.len(),
            RecordType::Problem => inner.problems.len(),
            RecordType::Analysis => inner.analyses.len(),
            RecordType::Decision => inner.decisions.len(),
            RecordType::Insight => inner.insights.len(),
            RecordType::FeatureRequest => 0, // FR-Records not stored in-memory store
        }
    }

    /// Save all records to a directory as JSON files.
    ///
    /// Creates subdirectories for each record type.
    pub fn save_to_directory(&self, dir: &Path) -> std::io::Result<()> {
        let inner = self.inner.read().unwrap();
        save_records_to_subdir(dir, "observations", &inner.observations)?;
        save_records_to_subdir(dir, "problems", &inner.problems)?;
        save_records_to_subdir(dir, "analyses", &inner.analyses)?;
        save_records_to_subdir(dir, "decisions", &inner.decisions)?;
        save_records_to_subdir(dir, "insights", &inner.insights)?;
        Ok(())
    }
}

#[allow(deprecated)]
impl Default for RecordStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn test_store_insert_and_retrieve() {
        let store = RecordStore::new();

        let obs = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "test"),
            source: "test".into(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT 1".into(),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({}),
        };

        let id = obs.header.id.clone();
        store.insert_observation(obs);

        let retrieved = store.get_observation(&id).unwrap();
        assert_eq!(retrieved.source, "test");
    }

    #[test]
    fn test_ranked_insights() {
        let store = RecordStore::new();

        let low = InsightRecord {
            header: RecordHeader {
                id: "I-2024-low0".into(),
                record_type: RecordType::Insight,
                timestamp: chrono::Utc::now(),
                created_by: "test".into(),
                derived_from: None,
                status: RecordStatus::Open,
            },
            category: InsightCategory::Friction,
            signal: InsightSignal {
                intent: "low priority".into(),
                volume: 10,
                success_rate: 0.5,
                trend: Trend::Stable,
                growth_rate: None,
            },
            recommendation: "fix later".into(),
            priority_score: 0.3,
        };

        let high = InsightRecord {
            header: RecordHeader {
                id: "I-2024-high".into(),
                record_type: RecordType::Insight,
                timestamp: chrono::Utc::now(),
                created_by: "test".into(),
                derived_from: None,
                status: RecordStatus::Open,
            },
            category: InsightCategory::UnmetIntent,
            signal: InsightSignal {
                intent: "high priority".into(),
                volume: 500,
                success_rate: 0.1,
                trend: Trend::Growing,
                growth_rate: Some(0.2),
            },
            recommendation: "build now".into(),
            priority_score: 0.9,
        };

        store.insert_insight(low);
        store.insert_insight(high);

        let ranked = store.ranked_insights();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].signal.intent, "high priority");
        assert_eq!(ranked[1].signal.intent, "low priority");
    }

    #[test]
    fn test_store_count() {
        let store = RecordStore::new();
        assert_eq!(store.count(RecordType::Observation), 0);

        store.insert_observation(ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "test"),
            source: "test".into(),
            classification: ObservationClass::ErrorRate,
            evidence_query: "SELECT 1".into(),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({}),
        });

        assert_eq!(store.count(RecordType::Observation), 1);
    }

    #[test]
    fn test_save_to_directory() {
        let store = RecordStore::new();

        let obs = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "test"),
            source: "test-save".into(),
            classification: ObservationClass::Performance,
            evidence_query: "SELECT 1".into(),
            threshold_field: None,
            threshold_value: None,
            observed_value: Some(42.0),
            context: serde_json::json!({}),
        };
        let obs_id = obs.header.id.clone();
        store.insert_observation(obs);

        let insight = InsightRecord {
            header: RecordHeader::new(RecordType::Insight, "test"),
            category: InsightCategory::UnmetIntent,
            signal: InsightSignal {
                intent: "test insight".into(),
                volume: 100,
                success_rate: 0.5,
                trend: Trend::Stable,
                growth_rate: None,
            },
            recommendation: "do something".into(),
            priority_score: 0.7,
        };
        let insight_id = insight.header.id.clone();
        store.insert_insight(insight);

        let tmp = std::env::temp_dir().join("temper-store-test");
        let _ = std::fs::remove_dir_all(&tmp);
        store.save_to_directory(&tmp).unwrap();

        assert!(tmp.join("observations").is_dir());
        assert!(tmp.join("problems").is_dir());
        assert!(tmp.join("analyses").is_dir());
        assert!(tmp.join("decisions").is_dir());
        assert!(tmp.join("insights").is_dir());

        let obs_path = tmp.join("observations").join(format!("{obs_id}.json"));
        assert!(obs_path.exists(), "Observation file should exist");
        let content = std::fs::read_to_string(&obs_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["source"], "test-save");

        let insight_path = tmp.join("insights").join(format!("{insight_id}.json"));
        assert!(insight_path.exists(), "Insight file should exist");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
