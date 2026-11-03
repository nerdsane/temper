//! The review an install asks for: every changed module at once, each answer cached by what was read.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::join_all;
use sha2::{Digest, Sha256};

use super::items::QUESTION_VERSION;
use super::questions::runners;
use super::review::review_module;
use super::types::{Judge, JudgeUnavailable, ModuleReview, ModuleSource, SpecSource};

/// How long one module's review may take, all its questions included: three rounds (rate, part, line) at Jev's usual
/// answer of under three seconds. A start and a Genesis install wait for their review, and a supervisor may give a new
/// version little time to serve (Katagami gives 20 seconds), so past this the review says Jev did not answer instead of
/// waiting on.
const MODULE_DEADLINE: Duration = Duration::from_secs(10);

/// Reviews kept. An install reviews only modules whose code changed, so the cache serves the same bundle again (a
/// retried install, another tenant); a review is a few kilobytes, so this stays under a megabyte.
const MAX_CACHED_REVIEWS: usize = 256;

/// What an install's review found, and why any module went unreviewed, said once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BundleReview {
    pub reviews: Vec<ModuleReview>,
    pub unavailable: Option<JudgeUnavailable>,
}

#[derive(Default)]
struct Cache {
    order: VecDeque<String>,
    reviews: BTreeMap<String, ModuleReview>,
}

/// Reviews modules with one judge and remembers its answers.
pub struct ModuleReviewer {
    judge: Arc<dyn Judge>,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for ModuleReviewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleReviewer").finish_non_exhaustive()
    }
}

impl ModuleReviewer {
    pub fn new(judge: Arc<dyn Judge>) -> Self {
        Self {
            judge,
            cache: Mutex::new(Cache::default()),
        }
    }

    /// Review `modules` against `specs`, all at once. A module whose review fails is left out and the first reason is
    /// kept for the install to say; a review never fails the install.
    pub async fn review(&self, modules: &[ModuleSource], specs: &[SpecSource]) -> BundleReview {
        let reviews = join_all(modules.iter().map(|module| self.review_one(module, specs))).await;
        let mut out = BundleReview::default();
        for review in reviews {
            match review {
                Ok(review) => out.reviews.push(review),
                Err(reason) => {
                    out.unavailable.get_or_insert(reason);
                }
            }
        }
        debug_assert!(out.reviews.len() <= modules.len());
        out
    }

    async fn review_one(
        &self,
        module: &ModuleSource,
        specs: &[SpecSource],
    ) -> Result<ModuleReview, JudgeUnavailable> {
        let key = cache_key(module, specs);
        if let Some(review) = self.cached(&key) {
            return Ok(review);
        }
        let review = tokio::time::timeout(
            MODULE_DEADLINE,
            review_module(self.judge.as_ref(), module, specs),
        )
        .await
        .map_err(|_| JudgeUnavailable::Down("deadline".to_string()))??;
        self.remember(key, &review);
        Ok(review)
    }

    fn cached(&self, key: &str) -> Option<ModuleReview> {
        let cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.reviews.get(key).cloned()
    }

    fn remember(&self, key: String, review: &ModuleReview) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.reviews.contains_key(&key) {
            return;
        }
        if cache.order.len() >= MAX_CACHED_REVIEWS
            && let Some(oldest) = cache.order.pop_front()
        {
            cache.reviews.remove(&oldest);
        }
        cache.order.push_back(key.clone());
        cache.reviews.insert(key, review.clone());
        debug_assert!(cache.reviews.len() <= MAX_CACHED_REVIEWS);
        debug_assert_eq!(cache.reviews.len(), cache.order.len());
    }
}

/// What a review read: the question wording, the module's name, path and source, and the specs that run it.
fn cache_key(module: &ModuleSource, specs: &[SpecSource]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(QUESTION_VERSION.to_le_bytes());
    for part in [&module.module, &module.path, &module.source] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    for spec in runners(&module.module, specs).specs {
        hasher.update((spec.source.len() as u64).to_le_bytes());
        hasher.update(spec.source.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}
