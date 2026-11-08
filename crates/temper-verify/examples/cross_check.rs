//! Check every rule that reads another record, by induction with Z3.
//!
//! ```text
//! cargo run -p temper-verify --example cross_check -- --specs DIR \
//!     [--cedar DIR] [--only Type.Rule] [--records N] [--limit-secs N] \
//!     [--fact Type#id=Status[,Status]]... [--rule 'Type.Name: expr']... \
//!     [--link 'Type.Name: expr']... [--json] [--detail] [--no-links]
//! ```
//!
//! `--only` shows one rule; every rule is still checked, because each may rest
//! on the others.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use temper_authz::{Asker, May, SendAnalysis};
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_spec::predicate::parse;
use temper_verify::smt::cross::{
    CrossFact, CrossOptions, CrossRule, CrossVerdict, check_cross_rules, cross_rules, link_rules,
};

#[derive(Default)]
struct Options {
    specs: PathBuf,
    cedar: Option<PathBuf>,
    only: Option<String>,
    records: usize,
    limit: u64,
    facts: Vec<CrossFact>,
    rules: Vec<CrossRule>,
    json: bool,
    detail: bool,
    no_links: bool,
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        limit: 120,
        ..Options::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--specs" => options.specs = value()?.into(),
            "--cedar" => options.cedar = Some(value()?.into()),
            "--only" => options.only = Some(value()?),
            "--records" => options.records = value()?.parse().map_err(|e| format!("{e}"))?,
            "--limit-secs" => options.limit = value()?.parse().map_err(|e| format!("{e}"))?,
            "--fact" => options.facts.push(fact(&value()?)?),
            "--rule" => options.rules.push(rule(&value()?)?),
            "--link" => {
                let mut link = rule(&value()?)?;
                link.link = true;
                options.rules.push(link);
            }
            "--json" => options.json = true,
            "--detail" => options.detail = true,
            "--no-links" => options.no_links = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.specs.as_os_str().is_empty() {
        return Err("--specs is required".into());
    }
    Ok(options)
}

fn fact(text: &str) -> Result<CrossFact, String> {
    let (record, statuses) = text.split_once('=').ok_or("--fact Type#id=Status")?;
    let (ty, id) = record.split_once('#').ok_or("--fact Type#id=Status")?;
    Ok(CrossFact {
        ty: ty.into(),
        id: id.into(),
        statuses: statuses.split(',').map(str::to_string).collect(),
    })
}

fn rule(text: &str) -> Result<CrossRule, String> {
    let (name, expr) = text.split_once(": ").ok_or("--rule 'Type.Name: expr'")?;
    let (owner, name) = name.split_once('.').ok_or("--rule 'Type.Name: expr'")?;
    Ok(CrossRule {
        owner: owner.into(),
        name: name.into(),
        assert: parse(expr).map_err(|e| e.to_string())?,
        link: false,
    })
}

fn load(options: &Options) -> Result<BTreeMap<String, Automaton>, String> {
    let mut specs = BTreeMap::new();
    for entry in fs::read_dir(&options.specs)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let path = entry.path();
        if path.to_string_lossy().ends_with(".ioa.toml") {
            let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let spec = parse_automaton(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            specs.insert(spec.automaton.name.clone(), spec);
        }
    }
    Ok(specs)
}

fn policies(options: &Options) -> Result<Option<SendAnalysis>, String> {
    let Some(dir) = &options.cedar else {
        return Ok(None);
    };
    let mut text = String::new();
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".cedar"))
        .collect();
    files.sort();
    for file in files {
        text.push_str(&fs::read_to_string(file).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    SendAnalysis::new(&text)
        .map(Some)
        .map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let options = options()?;
    let specs = load(&options)?;
    let analysis = policies(&options)?;
    let may_send = |ty: &str, action: &str| {
        analysis.as_ref().is_none_or(|analysis| {
            analysis.may_request(&Asker::Outside, action, ty, &|_| Ok(())) != May::No
        })
    };
    let mut rules = cross_rules(&specs);
    rules.extend(options.rules.iter().cloned());
    if !options.no_links {
        rules.extend(link_rules(&specs));
    }
    let check = CrossOptions {
        records: options.records,
        facts: options.facts.clone(),
        may_send: Some(&may_send),
        limit: Some(Duration::from_secs(options.limit)),
    };
    let mut out = std::io::stdout().lock();
    for verdict in check_cross_rules(&specs, &rules, &check) {
        if options
            .only
            .as_deref()
            .is_some_and(|only| only != verdict.rule)
        {
            continue;
        }
        show(&mut out, &verdict, &options).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn show(out: &mut impl Write, verdict: &CrossVerdict, options: &Options) -> std::io::Result<()> {
    let shown = &verdict.rule;
    if options.json {
        return writeln!(out, "{}", serde_json::to_string(verdict)?);
    }
    let said = match (verdict.proved, verdict.cut) {
        (true, _) if verdict.paths == 0 => "PROVED, nothing can send a move that breaks it",
        (true, _) => "PROVED",
        (false, true) => "CUT",
        (false, false) => "NOT INDUCTIVE",
    };
    if verdict.link && !options.detail {
        return writeln!(out, "{shown}: {said}, {:.2} s", verdict.secs);
    }
    writeln!(
        out,
        "{shown}: {said}, {} moves, {} paths, {} queries, {:.2} s, {} counterexamples{}",
        verdict.moves,
        verdict.paths,
        verdict.queries,
        verdict.secs,
        verdict.breaks.len(),
        if verdict.chains_cut {
            ", chains cut"
        } else {
            ""
        },
    )?;
    writeln!(out, "  can make it false: {}", verdict.breakers.join(", "))?;
    writeln!(out, "  can make it true: {}", verdict.fixers.join(", "))?;
    for found in &verdict.breaks {
        writeln!(
            out,
            "  {} at {}, {}: {}",
            found.record,
            found.at,
            if found.from_start {
                "from a start"
            } else {
                "free sender"
            },
            found.key
        )?;
        if options.detail {
            for step in &found.steps {
                writeln!(out, "    {step}")?;
            }
            for record in &found.records {
                writeln!(out, "      {record}")?;
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
