//! A write that hangs: the store neither writes it nor answers until it is
//! let go, as a database whose connection stalls in the middle of a commit.
//! Its caller sees a timeout, not a refusal.

use tokio::sync::watch;

use super::SimEventStore;

/// The append set to hang, and whether hung appends are held.
#[derive(Debug)]
pub(super) struct Hang {
    /// The journal whose next append hangs, until one does.
    next: Option<String>,
    /// Whether an append has hung since the last let-go.
    met: bool,
    /// `true` while hung appends are held.
    held: watch::Sender<bool>,
}

impl Default for Hang {
    fn default() -> Self {
        Self {
            next: None,
            met: false,
            held: watch::Sender::new(false),
        }
    }
}

impl SimEventStore {
    /// Make the next append that writes `persistence_id`, alone or in a
    /// batch, hang: it writes nothing and does not answer until
    /// [`Self::let_hung_appends_go`]. It then goes on as if just sent, so it
    /// is written if the store takes writes by then and refused if it does
    /// not. Reads and other appends go on while it hangs.
    pub fn hang_next_append(&self, persistence_id: &str) {
        let mut inner = self.lock();
        inner.hang.next = Some(persistence_id.to_string());
        inner.hang.held.send_replace(true);
    }

    /// Let every hung append go on, cancel a hang that has not happened yet,
    /// and say whether an append hung, so a test knows the hang met a write.
    pub fn let_hung_appends_go(&self) -> bool {
        let mut inner = self.lock();
        inner.hang.next = None;
        inner.hang.held.send_replace(false);
        std::mem::take(&mut inner.hang.met)
    }

    /// Hold an append that writes `persistence_ids` while the next append to
    /// one of them is set to hang, until it is let go.
    pub(super) async fn hang_if_set<'a>(&self, persistence_ids: impl IntoIterator<Item = &'a str>) {
        let mut held = {
            let mut inner = self.lock();
            let hangs = inner
                .hang
                .next
                .as_deref()
                .is_some_and(|next| persistence_ids.into_iter().any(|id| id == next));
            if !hangs {
                return;
            }
            inner.hang.next = None;
            inner.hang.met = true;
            inner.hang.held.subscribe()
        };
        // Waited on without the store's lock, so reads and other writes go on.
        // A store dropped while the append hangs lets it go.
        let _ = held.wait_for(|held| !held).await;
    }
}
