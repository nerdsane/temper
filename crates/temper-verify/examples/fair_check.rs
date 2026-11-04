//! Print what the joint checks find for every `eventually` rule of some specs.
//!
//! ```text
//! cargo run -p temper-verify --example fair_check -- --specs DIR \
//!     [--cedar DIR] [--all-fair] [--only Type.Rule] [--room-gb N] [--threads N] \
//!     [--json] [--models]
//! ```
//!
//! `--specs` holds `*.ioa.toml`, all one app. With `--models`, each name before
//! its first `.` is an app of its own (`tests/fair_models`), with `<app>.cedar`
//! beside it as its Cedar if there is one. `--all-fair` runs
//! the fairness check on rules that do not ask for it, assuming only what the
//! runtime owes.
//! `--json` prints one line per rule: what `scripts/fair-cross-check.mjs` reads.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Fairness, parse_automaton};
use temper_verify::composite::{
    Bound, Bytes, FairOutcome, LivenessOutcome, Progress, liveness_proofs,
};

struct Options {
    specs: PathBuf,
    cedar: Option<PathBuf>,
    all_fair: bool,
    only: Option<String>,
    room: u64,
    threads: usize,
    json: bool,
    models: bool,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        specs: PathBuf::new(),
        cedar: None,
        all_fair: false,
        only: None,
        room: 8,
        threads: 1,
        json: false,
        models: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--specs" => options.specs = value()?.into(),
            "--cedar" => options.cedar = Some(value()?.into()),
            "--only" => options.only = Some(value()?),
            "--room-gb" => {
                options.room = value()?.parse().map_err(|e| format!("--room-gb: {e}"))?
            }
            "--threads" => {
                options.threads = value()?.parse().map_err(|e| format!("--threads: {e}"))?
            }
            "--all-fair" => options.all_fair = true,
            "--json" => options.json = true,
            "--models" => options.models = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.specs.as_os_str().is_empty() {
        return Err("--specs is required".into());
    }
    Ok(options)
}

/// The files directly in `dir` whose name ends in `suffix`, sorted.
fn files(dir: &Path, suffix: &str) -> Result<Vec<PathBuf>, String> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(suffix))
        .collect();
    found.sort();
    Ok(found)
}

fn parse(path: &Path, all_fair: bool) -> Result<Automaton, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut automaton = parse_automaton(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if all_fair {
        for live in automaton
            .liveness
            .iter_mut()
            .filter(|live| live.eventually.is_some())
        {
            live.fairness.get_or_insert_with(Fairness::default);
        }
    }
    Ok(automaton)
}

fn check_app(
    label: &str,
    automatons: &[Automaton],
    policies: Option<&SendAnalysis>,
    options: &Options,
) {
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let room = Bytes::new((options.room as usize) << 30).with_threads(options.threads);
    let progress = Progress::default();
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    for proof in liveness_proofs(&refs, policies, &room) {
        let name = temper_verify::composite::JointProof::Liveness(proof);
        let shown = name.name();
        if options.only.as_deref().is_some_and(|only| only != shown) {
            continue;
        }
        let temper_verify::composite::JointProof::Liveness(proof) = name else {
            continue;
        };
        let start = Instant::now();
        let result = proof.check(bound);
        if !options.json {
            println!("{result}\n  ({:.1?})\n", start.elapsed());
            continue;
        }
        let can_reach = match &result.outcome {
            LivenessOutcome::Holds => "holds",
            LivenessOutcome::Stuck { .. } => "broken",
            LivenessOutcome::Vacuous { .. } => "vacuous",
            LivenessOutcome::Incomplete { .. } => "not-checked",
        };
        let (left_alone, with_people) = match result.fair.as_ref().map(|fair| &fair.outcome) {
            None => ("not-asked", "not-asked"),
            Some(FairOutcome::Holds {
                kept_away: None,
                may_be_kept_away: None,
            }) => ("holds", "holds"),
            Some(FairOutcome::Holds {
                kept_away: None,
                may_be_kept_away: Some(_),
            }) => ("holds", "not-checked"),
            Some(FairOutcome::Holds {
                kept_away: Some(_), ..
            }) => ("holds", "broken"),
            Some(FairOutcome::Broken { .. }) => ("broken", "broken"),
            Some(FairOutcome::Vacuous { .. }) => ("vacuous", "vacuous"),
            Some(FairOutcome::Incomplete { .. }) => ("not-checked", "not-checked"),
        };
        println!(
            "{}",
            serde_json::json!({
                "model": label,
                "rule": shown,
                "states": result.states,
                "seconds": start.elapsed().as_secs_f64(),
                "can_reach": can_reach,
                "left_alone": left_alone,
                "with_people": with_people,
                "text": result.to_string(),
            })
        );
    }
}

fn main() -> Result<(), String> {
    let options = options()?;
    let policies = match &options.cedar {
        Some(dir) => {
            let mut text = String::new();
            for path in files(dir, ".cedar")? {
                text += &fs::read_to_string(&path).map_err(|e| e.to_string())?;
                text.push('\n');
            }
            Some(SendAnalysis::new(&text).map_err(|e| e.to_string())?)
        }
        None => None,
    };
    let specs = files(&options.specs, ".ioa.toml")?;
    let mut apps: BTreeMap<String, Vec<Automaton>> = BTreeMap::new();
    for path in specs {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let app = if options.models {
            name.split('.').next().unwrap_or_default()
        } else {
            "app"
        };
        apps.entry(app.to_string())
            .or_default()
            .push(parse(&path, options.all_fair)?);
    }
    for (app, automatons) in &apps {
        let own = options.specs.join(format!("{app}.cedar"));
        let own = match options.models && own.exists() {
            true => {
                let text = fs::read_to_string(&own).map_err(|e| e.to_string())?;
                Some(SendAnalysis::new(&text).map_err(|e| e.to_string())?)
            }
            false => None,
        };
        check_app(
            app,
            automatons,
            own.as_ref().or(policies.as_ref()),
            &options,
        );
    }
    Ok(())
}
