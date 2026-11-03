//! The module reviews running in this process, one per app of a tenant. An install's answer does not wait for its
//! review, so this is how a later question learns a review is still reading.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::task::{AbortHandle, JoinHandle};

struct Running {
    turn: u64,
    version: String,
    abort: AbortHandle,
    failure: Option<ReviewSaveFailure>,
}

/// Findings retained in this process because the durable store rejected them.
#[derive(Debug, Clone)]
pub(crate) struct ReviewSaveFailure {
    pub(crate) result: serde_json::Value,
    pub(crate) error: String,
}

/// A snapshot for polling, including an unsaved result after a failed save.
#[derive(Debug, Clone)]
pub(crate) enum ReviewStatus {
    Reading,
    SaveFailed(ReviewSaveFailure),
}

#[derive(Default)]
struct Reviews {
    turns: u64,
    running: BTreeMap<(String, String), Running>,
}

/// The reviews under way.
#[derive(Default)]
pub struct ReviewsUnderWay(Mutex<Reviews>);

impl std::fmt::Debug for ReviewsUnderWay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewsUnderWay").finish_non_exhaustive()
    }
}

/// Remove finished, stopped or panicked work, retaining a failed save until superseded.
struct Leaving {
    reviews: Arc<ReviewsUnderWay>,
    key: (String, String),
    turn: u64,
}

impl Drop for Leaving {
    fn drop(&mut self) {
        let mut reviews = self
            .reviews
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if reviews
            .running
            .get(&self.key)
            .is_some_and(|running| running.turn == self.turn && running.failure.is_none())
        {
            reviews.running.remove(&self.key);
        }
    }
}

impl ReviewsUnderWay {
    /// Run `review` for `version` of `app`, stopping the app's review before it: only the newest is kept, so the older
    /// one's paid calls would be for nothing.
    pub(crate) fn start<F>(
        self: &Arc<Self>,
        tenant: &str,
        app: &str,
        version: &str,
        review: F,
    ) -> JoinHandle<()>
    where
        F: Future<Output = Result<(), ReviewSaveFailure>> + Send + 'static,
    {
        let key = (tenant.to_string(), app.to_string());
        let mut reviews = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        reviews.turns += 1;
        let turn = reviews.turns;
        let leaving = Leaving {
            reviews: Arc::clone(self),
            key: key.clone(),
            turn,
        };
        let task = tokio::spawn(async move {
            let registration = leaving;
            if let Err(failure) = review.await {
                let mut reviews = registration
                    .reviews
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(running) = reviews
                    .running
                    .get_mut(&registration.key)
                    .filter(|running| running.turn == turn)
                {
                    running.failure = Some(failure);
                }
            }
        });
        let running = Running {
            turn,
            version: version.to_string(),
            abort: task.abort_handle(),
            failure: None,
        };
        let older = reviews.running.insert(key, running);
        drop(reviews);
        if let Some(older) = older {
            older.abort.abort();
        }
        task
    }

    /// Whether a review of `version` of `app` is reading or saving now.
    pub fn reading(&self, tenant: &str, app: &str, version: &str) -> bool {
        matches!(
            self.status(tenant, app, version),
            Some(ReviewStatus::Reading)
        )
    }

    /// Read running or failed-save state atomically; successful results are in the store.
    pub(crate) fn status(&self, tenant: &str, app: &str, version: &str) -> Option<ReviewStatus> {
        let reviews = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        reviews
            .running
            .get(&(tenant.to_string(), app.to_string()))
            .filter(|running| running.version == version)
            .map(|running| match &running.failure {
                Some(failure) => ReviewStatus::SaveFailed(failure.clone()),
                None => ReviewStatus::Reading,
            })
    }
}
