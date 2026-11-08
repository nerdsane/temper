//! `temper sync-copy`: inspect or repair a database whose changes are copied
//! to Turso Cloud (`TURSO_SYNC_URL`).
//!
//! When the remote refuses a change it still commits the rest of that push,
//! so it is missing one, and the server stops confirming writes (it answers
//! "SavingStopped") until an operator acts, with the app stopped:
//!
//! - `status` shows the stop and how far the file and its last push got.
//! - If the file is right: rebuild the remote from a copy of it (drop the
//!   copy's `turso_cdc` and `turso_cdc_version` tables, upload it as the app's
//!   Turso database), then `restore` so the next start downloads that copy
//!   and gives the file fresh sync records.
//! - If the remote is right: `restore` alone; the file is moved aside (kept
//!   for inspection) and the next start downloads the remote.
//! - If the stop was raised in error and the remote holds every change:
//!   `resume` clears it and the next start confirms writes again.
//!
//! A restore runs for as long as it keeps writing the file. One that stopped
//! part way (`status` shows "restoring") needs nothing: the next start moves
//! the partial file aside, keeps it, and restores again.
//!
//! `fetch` downloads the remote into a new path, with TURSO_SYNC_URL and
//! TURSO_SYNC_AUTH_TOKEN set, the way a start restores it, printing how far it
//! has got as it goes. It never overwrites (it refuses a path where the file
//! or any `<path>-*` exists), serves nothing and leaves the remote unchanged.
//! Run it in place of a start, with the app stopped, then move the fetched
//! files into place by hand.
//!
//! `adopt` moves a database that was never synced onto an empty remote, with
//! the app stopped and TURSO_SYNC_URL and TURSO_SYNC_AUTH_TOKEN set: the file
//! is set aside and kept, and its rows are copied into a fresh synced
//! database, pushed chunk by chunk.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use temper_store_turso::SyncCopy;

#[derive(clap::Subcommand)]
pub enum Action {
    /// Show the stop, if any, and how far the file and its last push got.
    Status {
        /// The database file (the app's TURSO_URL, `file:` optional).
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: clear the stop so the next start confirms writes again.
    Resume {
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: move the file aside so the next start restores it from the remote.
    Restore {
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: copy a never-synced database onto the empty remote
    /// named by TURSO_SYNC_URL and TURSO_SYNC_AUTH_TOKEN, keeping the original aside.
    Adopt {
        #[arg(long)]
        db: String,
    },
    /// With the app stopped: download the remote named by TURSO_SYNC_URL and
    /// TURSO_SYNC_AUTH_TOKEN into a new path, never over an existing file.
    Fetch {
        #[arg(long)]
        db: String,
    },
}

/// How often `fetch` prints how far it has got.
const FETCH_REPORT_EVERY: Duration = Duration::from_secs(30);

fn remote() -> Result<(String, String)> {
    let url = std::env::var("TURSO_SYNC_URL").context("TURSO_SYNC_URL is required")?;
    let token =
        std::env::var("TURSO_SYNC_AUTH_TOKEN").context("TURSO_SYNC_AUTH_TOKEN is required")?;
    Ok((url, token))
}

async fn fetch(db: &str) -> Result<String> {
    let (url, token) = remote()?;
    let mut fetching = std::pin::pin!(SyncCopy::fetch(db, &url, &token));
    let mut report = tokio::time::interval(FETCH_REPORT_EVERY);
    report.tick().await;
    loop {
        tokio::select! {
            fetched = &mut fetching => return Ok(fetched?),
            _ = report.tick() => {
                let (written, size) = SyncCopy::restored_bytes(db);
                writeln!(std::io::stdout(), "fetching: {written} of {size} bytes written")?;
            }
        }
    }
}

pub async fn run(action: Action) -> Result<()> {
    let report = match action {
        Action::Status { db } => SyncCopy::describe(&db)?,
        Action::Resume { db } => match SyncCopy::clear_stop(&db)? {
            Some(cause) => format!("cleared the stop ({cause}); the next start confirms writes"),
            None => format!("{db} is not stopped"),
        },
        Action::Restore { db } => format!(
            "moved to {}; the next start restores the database from its remote copy",
            SyncCopy::set_aside(&db)?.display()
        ),
        Action::Adopt { db } => {
            let (url, token) = remote()?;
            temper_store_turso::store::sync_adopt::adopt(&db, &url, &token).await?
        }
        Action::Fetch { db } => fetch(&db).await?,
    };
    writeln!(std::io::stdout(), "{report}")?;
    Ok(())
}
