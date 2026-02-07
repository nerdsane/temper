//! Prompt version registry for A/B testing agent prompts.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Tracks prompt versions and their performance metrics.
pub struct PromptRegistry {
    versions: Arc<RwLock<HashMap<String, PromptVersion>>>,
    active_version: Arc<RwLock<String>>,
}

/// A versioned prompt with usage statistics.
pub struct PromptVersion {
    /// Unique version identifier.
    pub version_id: String,
    /// The system prompt text.
    pub system_prompt: String,
    /// Total number of times this version has been used.
    pub total_uses: u64,
    /// Number of successful completions.
    pub success_count: u64,
    /// Running average of turns per conversation.
    pub avg_turns: f64,
}

impl PromptRegistry {
    /// Create a new registry with an initial prompt version.
    pub fn new(initial_version: &str, initial_prompt: &str) -> Self {
        let mut versions = HashMap::new();
        versions.insert(
            initial_version.to_string(),
            PromptVersion {
                version_id: initial_version.to_string(),
                system_prompt: initial_prompt.to_string(),
                total_uses: 0,
                success_count: 0,
                avg_turns: 0.0,
            },
        );
        Self {
            versions: Arc::new(RwLock::new(versions)),
            active_version: Arc::new(RwLock::new(initial_version.to_string())),
        }
    }

    /// Get the currently active prompt version ID and text.
    pub fn get_active_prompt(&self) -> (String, String) {
        let version_id = self.active_version.read().unwrap().clone();
        let versions = self.versions.read().unwrap();
        let prompt = versions
            .get(&version_id)
            .map(|v| v.system_prompt.clone())
            .unwrap_or_default();
        (version_id, prompt)
    }

    /// Register a new prompt version.
    pub fn register_version(&self, version_id: &str, prompt: &str) {
        self.versions
            .write()
            .unwrap()
            .insert(
                version_id.to_string(),
                PromptVersion {
                    version_id: version_id.to_string(),
                    system_prompt: prompt.to_string(),
                    total_uses: 0,
                    success_count: 0,
                    avg_turns: 0.0,
                },
            );
    }

    /// Record a usage of a prompt version.
    pub fn record_use(&self, version_id: &str, success: bool, turns: u32) {
        if let Some(v) = self.versions.write().unwrap().get_mut(version_id) {
            v.total_uses += 1;
            if success {
                v.success_count += 1;
            }
            let n = v.total_uses as f64;
            v.avg_turns = v.avg_turns * ((n - 1.0) / n) + turns as f64 / n;
        }
    }

    /// Set the active prompt version.
    pub fn set_active(&self, version_id: &str) {
        *self.active_version.write().unwrap() = version_id.to_string();
    }
    /// Get stats for all versions.
    pub fn stats(&self) -> Vec<(String, u64, f64, f64)> {
        self.versions.read().unwrap().values().map(|v| {
            let sr = if v.total_uses > 0 { v.success_count as f64 / v.total_uses as f64 } else { 0.0 };
            (v.version_id.clone(), v.total_uses, sr, v.avg_turns)
        }).collect()
    }
}
