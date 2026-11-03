//! Test control over the one retry while acquiring the catalog lock.

#[cfg(not(test))]
pub(super) fn ran(_schema: &str) {}
#[cfg(not(test))]
pub(super) async fn before_retry(_schema: &str) {}
#[cfg(not(test))]
pub(super) fn takes_retry_lock(_schema: &str) -> bool {
    true
}

#[cfg(test)]
pub(crate) use testing::*;

/// Hooks into a raced append's retry, keyed by the store's schema so tests
/// running at once do not share them. A schema with no seam installed runs as
/// production does.
#[cfg(test)]
mod testing {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    /// What one test asks of the appends in its schema.
    #[derive(Default)]
    pub(crate) struct Seam {
        /// How many times the append statement ran.
        pub(crate) runs: usize,
        /// Leave out the retry's lock statement, so its run can race again.
        pub(crate) skip_retry_lock: bool,
        /// Told when a retry starts; the retry waits for the answer.
        pub(crate) before_retry: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    }

    static SEAMS: Mutex<BTreeMap<String, Seam>> = Mutex::new(BTreeMap::new());

    pub(crate) fn install(schema: &str, seam: Seam) {
        SEAMS.lock().unwrap().insert(schema.to_string(), seam);
    }

    pub(crate) fn runs(schema: &str) -> usize {
        SEAMS
            .lock()
            .unwrap()
            .get(schema)
            .map_or(0, |seam| seam.runs)
    }

    pub(crate) fn ran(schema: &str) {
        if let Some(seam) = SEAMS.lock().unwrap().get_mut(schema) {
            seam.runs += 1;
        }
    }

    pub(crate) async fn before_retry(schema: &str) {
        let pair = SEAMS
            .lock()
            .unwrap()
            .get_mut(schema)
            .and_then(|seam| seam.before_retry.take());
        if let Some((started, go)) = pair {
            let _ = started.send(());
            let _ = go.await;
        }
    }

    pub(crate) fn takes_retry_lock(schema: &str) -> bool {
        SEAMS
            .lock()
            .unwrap()
            .get(schema)
            .is_none_or(|seam| !seam.skip_retry_lock)
    }
}
