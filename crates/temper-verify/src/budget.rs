//! How much one verification cascade may spend, and what it says when a
//! spec is too large to check.
//!
//! Every counter, boolean and numeric action parameter multiplies the states
//! the model check walks and the successors it computes for each, so a small
//! change to a spec can turn seconds into hours. A server that verifies specs
//! it was sent gives each cascade a [`Budget`]: the level that runs out of it
//! stops, fails with a summary saying so, and the levels after it do not run.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cascade::{CascadeLevel, LevelResult};
use crate::model::{ExploreLimit, TemperModel};

/// The most one cascade may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Wall-clock time for the whole cascade.
    pub time: Duration,
    /// Generated states and expansion work allowed per walk. A state reached
    /// twice counts twice; intermediate parameter products also consume work.
    pub states: usize,
}

/// Where a budgeted cascade reads the time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Clock {
    /// The wall clock.
    #[default]
    Wall,
    /// A clock that moves only when the cascade reads it, by this much each
    /// time. A time budget then runs out after the same work on any machine,
    /// however fast or busy it is.
    Steps(Duration),
}

/// The time a budget has used, read from its [`Clock`].
#[derive(Debug, Clone)]
pub(crate) struct Elapsed {
    clock: Clock,
    started: Instant,
    readings: Arc<AtomicU64>,
}

impl Elapsed {
    pub(crate) fn start(clock: Clock) -> Self {
        Self {
            clock,
            started: Instant::now(),
            readings: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The time since the budget started. Reading a [`Clock::Steps`] moves
    /// it on by one step.
    pub(crate) fn now(&self) -> Duration {
        match self.clock {
            Clock::Wall => self.started.elapsed(),
            Clock::Steps(step) => {
                let readings = self.readings.fetch_add(1, Ordering::Relaxed) + 1;
                step.saturating_mul(u32::try_from(readings).unwrap_or(u32::MAX))
            }
        }
    }
}

/// A budget being spent, on its clock.
#[derive(Debug, Clone)]
pub(crate) struct Spending {
    budget: Budget,
    limit: ExploreLimit,
}

impl Spending {
    pub(crate) fn start(budget: Budget, clock: Clock) -> Self {
        let limit = ExploreLimit {
            elapsed: Elapsed::start(clock),
            time: budget.time,
            states: budget.states,
            lifted: Arc::new(AtomicBool::new(false)),
            work: Arc::new(std::sync::atomic::AtomicUsize::new(budget.states)),
            exhausted: Arc::new(AtomicBool::new(false)),
        };
        Self { budget, limit }
    }

    /// `model`, explored no further than this budget allows.
    pub(crate) fn limit(&self, mut model: TemperModel) -> TemperModel {
        model.limit = Some(self.limit.clone());
        model
    }

    pub(crate) fn spent(&self) -> bool {
        self.limit.past_time() || self.limit.exhausted.load(Ordering::Relaxed)
    }

    /// What a level cut short says, after `label` ("L1 Model Check").
    /// `states` is how many distinct states the model check explored, when it
    /// ran, and `found` the properties it found false before it stopped.
    pub(crate) fn stopped_summary(
        &self,
        label: &str,
        states: Option<usize>,
        found: &[String],
    ) -> String {
        let explored = states.map_or(String::new(), |n| {
            format!(" ({n} distinct states explored)")
        });
        let limit = if self.limit.past_time() {
            format!("ran out of its {} s budget", secs(self.budget.time))
        } else {
            format!(
                "reached its limit of {} generated states or expansion steps after {} s",
                self.budget.states,
                secs(self.limit.elapsed.now())
            )
        };
        let found = if found.is_empty() {
            String::new()
        } else {
            format!(", and found {} false before it stopped", found.join(", "))
        };
        format!(
            "{label} STOPPED: {limit}{explored}{found}; every counter, flag and numeric action \
             parameter multiplies the states to check, so use fewer of them or smaller bounds"
        )
    }
}

/// Whole seconds as they are, anything finer to a tenth.
fn secs(time: Duration) -> String {
    if time.subsec_millis() == 0 {
        time.as_secs().to_string()
    } else {
        format!("{:.1}", time.as_secs_f64())
    }
}

/// Whether the budget ran out during the last level: by time or expansion
/// work, or by a model check it left incomplete (`explored` is its
/// states and whether it finished). A level cut short fails and says so.
pub(crate) fn ran_out(
    spending: Option<&Spending>,
    levels: &mut [LevelResult],
    explored: Option<(usize, bool)>,
) -> bool {
    let Some(spending) = spending else {
        return false;
    };
    let unfinished = explored.is_some_and(|(_, complete)| !complete);
    let Some(level) = levels.last_mut() else {
        return false;
    };
    if !spending.spent() && !unfinished {
        return false;
    }
    let found: Vec<String> = level
        .verification
        .iter()
        .flat_map(|v| v.counterexamples.iter().map(|c| c.property.clone()))
        .collect();
    level.passed = false;
    level.summary =
        spending.stopped_summary(short_label(level.level), explored.map(|(n, _)| n), &found);
    true
}

/// The model check of a move with `choices` parameter assignments, more than
/// one step explores: failed before it starts.
pub(crate) fn too_many_choices(action: &str, choices: usize) -> LevelResult {
    LevelResult {
        level: CascadeLevel::ModelCheck,
        passed: false,
        summary: format!(
            "L1 Model Check FAILED: {action} takes {choices} combinations of parameter values, \
             more than the {} the model check tries from one state; every flag and numeric \
             parameter of one move multiplies them, so give it fewer",
            crate::model::semantics::MAX_PARAM_ASSIGNMENTS,
        ),
        diagnostics: Vec::new(),
        verification: None,
        simulation: None,
        prop_test: None,
        smt: None,
    }
}

/// A level's name as its summaries begin.
fn short_label(level: CascadeLevel) -> &'static str {
    match level {
        CascadeLevel::SymbolicVerification => "L0 Symbolic",
        CascadeLevel::ModelCheck => "L1 Model Check",
        CascadeLevel::Simulation => "L2 Simulation",
        CascadeLevel::ActorSimulation => "L2b Actor Simulation",
        CascadeLevel::PropertyTest => "L3 Property Tests",
    }
}
