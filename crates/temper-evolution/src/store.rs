//! In-memory record store for evolution records.
//! Production deployments would back this with Git + Postgres.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::records::*;

/// Stores evolution records. In-memory implementation for now.
/// Production: Git (source of truth) + Postgres (indexed for querying).
#[derive(Clone)]
pub struct RecordStore {
    inner: Arc<RwLock<StoreInner>>,
}

struct StoreInner {
    observations: HashMap<RecordId, ObservationRecord>,
    problems: HashMap<RecordId, ProblemRecord>,
    analyses: HashMap<RecordId, AnalysisRecord>,
    decisions: HashMap<RecordId, DecisionRecord>,
    insights: HashMap<RecordId, InsightRecord>,
}

impl RecordStore {
    /// Create a new, empty record store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(StoreInner {
                observations: HashMap::new(),
                problems: HashMap::new(),
                analyses: HashMap::new(),
                decisions: HashMap::new(),
                insights: HashMap::new(),
            })),
        }
    }

    // --- Insert ---

    /// Insert an observation record into the store.
    pub fn insert_observation(&self, record: ObservationRecord) {
        self.inner
            .write()
            .unwrap() // ci-ok: infallible lock
            .observations
            .insert(record.header.id.clone(), record);
    }

    /// Insert a problem record into the store.
    pub fn insert_problem(&self, record: ProblemRecord) {
        self.inner
            .write()
            .unwrap() // ci-ok: infallible lock
            .problems
            .insert(record.header.id.clone(), record);
    }

    /// Insert an analysis record into the store.
    pub fn insert_analysis(&self, record: AnalysisRecord) {
        self.inner
            .write()
            .unwrap() // ci-ok: infallible lock
            .analyses
            .insert(record.header.id.clone(), record);
    }

    /// Insert a decision record into the store.
    pub fn insert_decision(&self, record: DecisionRecord) {
        self.inner
            .write()
            .unwrap() // ci-ok: infallible lock
            .decisions
            .insert(record.header.id.clone(), record);
    }

    /// Insert an insight record into the store.
    pub fn insert_insight(&self, record: InsightRecord) {
        self.inner
            .write()
            .unwrap() // ci-ok: infallible lock
            .insights
            .insert(record.header.id.clone(), record);
    }

    // --- Query ---

    /// Retrieve an observation record by ID.
    pub fn get_observation(&self, id: &str) -> Option<ObservationRecord> {
        self.inner.read().unwrap().observations.get(id).cloned() // ci-ok: infallible lock
    }

    /// Retrieve a problem record by ID.
    pub fn get_problem(&self, id: &str) -> Option<ProblemRecord> {
        self.inner.read().unwrap().problems.get(id).cloned() // ci-ok: infallible lock
    }

    /// Retrieve an analysis record by ID.
    pub fn get_analysis(&self, id: &str) -> Option<AnalysisRecord> {
        self.inner.read().unwrap().analyses.get(id).cloned() // ci-ok: infallible lock
    }

    /// Retrieve a decision record by ID.
    pub fn get_decision(&self, id: &str) -> Option<DecisionRecord> {
        self.inner.read().unwrap().decisions.get(id).cloned() // ci-ok: infallible lock
    }

    /// Retrieve an insight record by ID.
    pub fn get_insight(&self, id: &str) -> Option<InsightRecord> {
        self.inner.read().unwrap().insights.get(id).cloned() // ci-ok: infallible lock
    }

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
        }
    }

    /// Save all records to a directory as JSON files.
    ///
    /// Creates subdirectories for each record type.
    pub fn save_to_directory(&self, dir: &Path) -> std::io::Result<()> {
        let inner = self.inner.read().unwrap();

        let sub_dir = dir.join("observations");
        fs::create_dir_all(&sub_dir)?;
        for (id, record) in &inner.observations {
            let path = sub_dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
            fs::write(&path, json)?;
        }

        let sub_dir = dir.join("problems");
        fs::create_dir_all(&sub_dir)?;
        for (id, record) in &inner.problems {
            let path = sub_dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
            fs::write(&path, json)?;
        }

        let sub_dir = dir.join("analyses");
        fs::create_dir_all(&sub_dir)?;
        for (id, record) in &inner.analyses {
            let path = sub_dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
            fs::write(&path, json)?;
        }

        let sub_dir = dir.join("decisions");
        fs::create_dir_all(&sub_dir)?;
        for (id, record) in &inner.decisions {
            let path = sub_dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
            fs::write(&path, json)?;
        }

        let sub_dir = dir.join("insights");
        fs::create_dir_all(&sub_dir)?;
        for (id, record) in &inner.insights {
            let path = sub_dir.join(format!("{id}.json"));
            let json = serde_json::to_string_pretty(record).map_err(std::io::Error::other)?;
            fs::write(&path, json)?;
        }

        Ok(())
    }
}

impl Default for RecordStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
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
                trend: "stable".into(),
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
                trend: "growing".into(),
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
                trend: "stable".into(),
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
