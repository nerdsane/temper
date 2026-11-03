//! Jev's review of an app's own modules against its specs, made at install.
//!
//! A module may only compute a value between two moves: parse, format, score, transform, or call one outside service
//! and return its answer. What else it does (choosing the move, keeping steps of its own, waiting, doing several
//! steps, holding rules, deciding out of sight, packing several concerns) belongs to the spec's states, guards, timers
//! and Cedar, where the checker can see it and the person can follow it.
//!
//! Advisory (Rita, 26 Oct 2026: warn, then migrate the apps, then enforce): a review never refuses or holds an install,
//! and a Jev that does not answer is said once in what the review keeps, never retried in a loop.

mod excerpt;
mod items;
mod jev;
mod questions;
mod review;
mod reviewer;
mod source;
mod types;
mod under_way;

pub use excerpt::{Excerpt, SPEC_BYTES, excerpt, said_left_out};
pub use items::{FOUND_AT, ITEMS, Item, LARGE_LINES, QUESTION_VERSION};
pub use jev::{JEV_KEY_ENV, JEV_MODEL, JEV_URL, JEV_URL_ENV, JevJudge};
pub use review::review_module;
pub use reviewer::{BundleReview, ModuleReviewer};
pub use types::{
    Answer, Answers, Finding, Judge, JudgeUnavailable, ModuleReview, ModuleSource, Question,
    SpecSource,
};
pub use under_way::ReviewsUnderWay;
pub(crate) use under_way::{ReviewSaveFailure, ReviewStatus};

#[cfg(test)]
mod tests;
