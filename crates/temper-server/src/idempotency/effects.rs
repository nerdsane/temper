//! One dispatcher at a time runs a keyed reply's post-dispatch effects.
//!
//! A retry can reach the actor while the first request's dispatcher is still
//! running the effects of the same reply (webhooks, integrations, spawns,
//! timers). The retry waits for that dispatcher instead of running them again.
//! When it finishes, the retry answers with the reply it completed. When it
//! ends without completing (an unsuccessful reply), the next dispatcher in line
//! runs them. A caller that hangs up does not end it: the dispatch runs on its
//! own task (`state::dispatch::detached`), so a retry after a hang-up waits for
//! the effects that dispatch is still running, as long as they take.

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::sync::watch;

use super::{IdempotencyCache, RequestFingerprint};
use crate::entity_actor::EntityResponse;

/// What a waiting retry learns from the dispatcher it waits for: the reply it
/// completed, or `None` while it runs and if it ends without completing.
type Finished = watch::Receiver<Option<EntityResponse>>;

/// The dispatchers running effects right now:
/// (actor_key, idempotency_key) → the channel that tells how that one ended.
#[derive(Default)]
pub(super) struct EffectsOwners(Mutex<BTreeMap<(String, String), Finished>>);

impl EffectsOwners {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), Finished>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether this dispatcher runs a reply's post-dispatch effects.
pub enum EffectsClaim<'a> {
    /// The effects have already run. Carries the reply the dispatcher that ran
    /// them completed. Boxed because a reply is much larger than an
    /// [`EffectsOwner`].
    Applied(Box<EntityResponse>),
    /// This dispatcher runs them, and completes the claim when they have run.
    Owner(EffectsOwner<'a>),
}

/// The right to run one reply's post-dispatch effects. Dropping it without
/// [`EffectsOwner::complete`] hands the effects to the next dispatcher.
pub struct EffectsOwner<'a> {
    cache: &'a IdempotencyCache,
    actor_key: String,
    idem_key: String,
    finished: watch::Sender<Option<EntityResponse>>,
}

impl EffectsOwner<'_> {
    /// Record that the effects ran, with the reply they produced. Retries of
    /// the same request are answered with it.
    ///
    /// The retries already waiting get the reply through the channel, not
    /// the cache: the cache may evict it (or not hold it at all) before they
    /// wake, and they must still see that the effects ran.
    pub fn complete(self, request: &RequestFingerprint, response: &EntityResponse) {
        self.cache
            .put_effects_applied(&self.actor_key, &self.idem_key, request, response);
        self.finished.send_replace(Some(response.clone()));
    }
}

impl Drop for EffectsOwner<'_> {
    fn drop(&mut self) {
        // The sender drops after this, which wakes every waiter; by then the
        // key has no owner, so a waiter that got no reply takes it over.
        self.cache
            .effects_owners
            .lock()
            .remove(&(self.actor_key.clone(), self.idem_key.clone()));
    }
}

/// What a retry answers once the reply's effects have run: the actor's own
/// reply while it is already at the state the effects completed, since the
/// actor answers a retry whose event is still newest with the live state in
/// full (the cached copy leaves out event parameters and the durable key
/// set); otherwise the completed reply, which the effects moved past the
/// actor's copy (an inline callback, for one, adds its own event).
pub fn retry_reply(actor_reply: EntityResponse, completed: EntityResponse) -> EntityResponse {
    if actor_reply.state.total_event_count >= completed.state.total_event_count {
        actor_reply
    } else {
        completed
    }
}

impl IdempotencyCache {
    /// Claim the post-dispatch effects of the successful reply for
    /// `(actor_key, idem_key)`. Waits while another dispatcher runs them.
    pub async fn claim_effects(&self, actor_key: &str, idem_key: &str) -> EffectsClaim<'_> {
        let owner_key = (actor_key.to_string(), idem_key.to_string());
        loop {
            let mut finished = {
                let mut owners = self.effects_owners.lock();
                if let Some(applied) = self.applied_reply(actor_key, idem_key) {
                    return EffectsClaim::Applied(Box::new(applied));
                }
                match owners.get(&owner_key) {
                    Some(finished) => finished.clone(),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        owners.insert(owner_key, receiver);
                        return EffectsClaim::Owner(EffectsOwner {
                            cache: self,
                            actor_key: actor_key.to_string(),
                            idem_key: idem_key.to_string(),
                            finished: sender,
                        });
                    }
                }
            };
            // Returns once the owner completes or its sender drops; the last
            // value stays readable after the sender is gone.
            let _ = finished.changed().await;
            if let Some(reply) = finished.borrow().clone() {
                return EffectsClaim::Applied(Box::new(reply));
            }
        }
    }
}
