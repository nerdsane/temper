//! In-memory record store for evolution records.
//! Production deployments would back this with Git + Postgres.

use std::collections::HashMap;
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

    pub fn insert_observation(&self, record: ObservationRecord) {
        self.inner.write().unwrap().observations.insert(record.header.id.clone(), record);
    }

    pub fn insert_problem(&self, record: ProblemRecord) {
        self.inner.write().unwrap().problems.insert(record.header.id.clone(), record);
    }

    pub fn insert_analysis(&self, record: AnalysisRecord) {
        self.inner.write().unwrap().analyses.insert(record.header.id.clone(), record);
    }

    pub fn insert_decision(&self, record: DecisionRecord) {
        self.inner.write().unwrap().decisions.insert(record.header.id.clone(), record);
    }

    pub fn insert_insight(&self, record: InsightRecord) {
        self.inner.write().unwrap().insights.insert(record.header.id.clone(), record);
    }

    // --- Query ---

    pub fn get_observation(&self, id: &str) -> Option<ObservationRecord> {
        self.inner.read().unwrap().observations.get(id).cloned()
    }

    pub fn get_problem(&self, id: &str) -> Option<ProblemRecord> {
        self.inner.read().unwrap().problems.get(id).cloned()
    }

    pub fn get_analysis(&self, id: &str) -> Option<AnalysisRecord> {
        self.inner.read().unwrap().analyses.get(id).cloned()
    }

    pub fn get_decision(&self, id: &str) -> Option<DecisionRecord> {
        self.inner.read().unwrap().decisions.get(id).cloned()
    }

    pub fn get_insight(&self, id: &str) -> Option<InsightRecord> {
        self.inner.read().unwrap().insights.get(id).cloned()
    }

    /// Get all open observations (not yet resolved).
    pub fn open_observations(&self) -> Vec<ObservationRecord> {
        self.inner.read().unwrap().observations.values()
            .filter(|r| r.header.status == RecordStatus::Open)
            .cloned()
            .collect()
    }

    /// Get all insights sorted by priority score (highest first).
    pub fn ranked_insights(&self) -> Vec<InsightRecord> {
        let mut insights: Vec<InsightRecord> = self.inner.read().unwrap()
            .insights.values()
            .filter(|r| r.header.status == RecordStatus::Open)
            .cloned()
            .collect();
        insights.sort_by(|a, b| b.priority_score.partial_cmp(&a.priority_score).unwrap_or(std::cmp::Ordering::Equal));
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
                volume: 10, success_rate: 0.5, trend: "stable".into(), growth_rate: None,
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
                volume: 500, success_rate: 0.1, trend: "growing".into(), growth_rate: Some(0.2),
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
}
