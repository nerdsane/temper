//! `temper audit`: re-check a spec's invariants against the entities that exist.
//!
//! `temper verify` proves things about a model. This asks whether the running
//! system currently agrees with it, which is a different question with
//! different failure modes -- see [`temper_jit::audit`] for why the two cannot
//! substitute for each other, and what each check means.
//!
//! The command is read-only. It issues GETs, writes nothing, and reports what
//! it found; deciding whether a finding means the spec is wrong or the data is
//! is left to the reader.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use temper_jit::audit::{AuditSeverity, EntitySnapshot, audit_entities};
use temper_spec::automaton::{Automaton, parse_automaton};

/// Entity sets are read one page at a time; this bounds a single request.
const PAGE_LIMIT: usize = 500;

/// Run the audit against a live server.
///
/// `token` and `tenant` are read from the environment rather than the command
/// line so a credential never lands in shell history or a process listing.
pub async fn run(specs_dir: &str, base_url: &str, tenant: Option<&str>) -> Result<()> {
    let specs_path = Path::new(specs_dir);
    let base = base_url.trim_end_matches('/');
    println!(
        "Audit\n  Specs directory: {}\n  Server: {base}",
        specs_path.display()
    );

    let automata = read_automata(specs_path)?;
    if automata.is_empty() {
        anyhow::bail!("no .ioa.toml files found in {}", specs_path.display());
    }

    let client = reqwest::Client::new();
    let token = std::env::var("TEMPER_TOKEN").ok();
    let tenant = tenant
        .map(str::to_string)
        .or_else(|| std::env::var("TEMPER_TENANT").ok());

    // Ask the server which entity sets exist rather than guessing names from
    // the specs. The set name comes from the CSDL's `entity_set_map`, which is
    // server-side configuration a spec file does not determine; pluralising
    // the entity type is only the server's own last-resort fallback.
    let sets = fetch_entity_sets(&client, base, token.as_deref(), tenant.as_deref()).await?;
    if sets.is_empty() {
        anyhow::bail!("server at {base} exposes no entity sets");
    }

    // Group rows by the `entity_type` each row carries, so a set whose name
    // does not resemble its type still lands on the right automaton.
    let mut by_type: BTreeMap<String, Vec<EntitySnapshot>> = BTreeMap::new();
    let mut undecodable = 0usize;
    let mut refused: Vec<&String> = Vec::new();
    for set in &sets {
        let Some(rows) =
            fetch_rows(&client, base, set, token.as_deref(), tenant.as_deref()).await?
        else {
            refused.push(set);
            continue;
        };
        for row in rows {
            let entity_type = row
                .get("entity_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(set)
                .to_string();
            match EntitySnapshot::from_tdata_row(&row) {
                Ok(snapshot) => by_type.entry(entity_type).or_default().push(snapshot),
                // A row the audit cannot read is reported rather than skipped
                // silently: a run that quietly audited nothing must not look
                // the same as a clean one.
                Err(error) => {
                    undecodable += 1;
                    eprintln!("  warning: unreadable row in '{set}': {error}");
                }
            }
        }
    }

    if refused.len() == sets.len() {
        anyhow::bail!(
            "every entity set on {base} refused the read ({} set(s)); nothing was audited",
            refused.len()
        );
    }
    let (output, violations) = render_reports(&automata, &by_type, &refused, undecodable);
    println!("{output}");
    if violations > 0 {
        // A non-zero exit so CI can gate on this. Warnings do not fail: they
        // are point-in-time observations, and failing on them would make the
        // command flaky by design.
        anyhow::bail!("{violations} invariant violation(s) in live data");
    }
    Ok(())
}

/// Render one complete report separately from terminal I/O so every finding
/// and coverage warning can be checked together.
fn render_reports(
    automata: &BTreeMap<String, Automaton>,
    by_type: &BTreeMap<String, Vec<EntitySnapshot>>,
    refused: &[&String],
    undecodable: usize,
) -> (String, usize) {
    let mut lines = Vec::new();
    if !refused.is_empty() {
        // Said once, plainly, rather than per set: on a stock server most of
        // these are Temper's own internals and the refusal is correct.
        lines.push(format!(
            "\n  sets not authorized for this token, not audited: {refused:?}"
        ));
    }

    let mut violations = 0usize;
    let mut warnings = 0usize;
    let mut audited = 0usize;
    for (name, automaton) in automata {
        let snapshots = by_type.get(name).map(Vec::as_slice).unwrap_or_default();
        let report = audit_entities(automaton, snapshots);
        audited += report.entities_audited;
        lines.push(format!(
            "\n  {name}: {} entities, {} with violations",
            report.entities_audited, report.entities_violating
        ));
        if snapshots.is_empty() {
            // Not a pass. An empty set proves nothing about the spec, and
            // saying "0 violations" without saying "0 entities" is how an
            // audit comes to be trusted for something it never checked.
            lines.push("        no entities of this type exist; nothing was checked".to_string());
            continue;
        }
        for finding in &report.findings {
            let label = match finding.severity {
                AuditSeverity::Violation => {
                    violations += 1;
                    "VIOLATION"
                }
                AuditSeverity::Warning => {
                    warnings += 1;
                    "WARNING"
                }
            };
            lines.push(format!(
                "    [{label}] {} ({}): {}",
                finding.entity_id, finding.status, finding.message
            ));
            if !finding.found.is_empty() {
                let read: Vec<String> = finding
                    .found
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect();
                lines.push(format!("           read: {}", read.join(", ")));
            }
        }
    }

    let unmatched: Vec<&String> = by_type
        .keys()
        .filter(|name| !automata.contains_key(*name))
        .collect();
    if !unmatched.is_empty() {
        lines.push(format!(
            "\n  entity types with no local spec, not audited: {unmatched:?}"
        ));
    }

    lines.push(format!(
        "\n  {audited} entities audited: {violations} violation(s), {warnings} warning(s)"
    ));
    if undecodable > 0 {
        lines.push(format!(
            "  {undecodable} row(s) could not be read and were not audited"
        ));
    }
    (lines.join("\n"), violations)
}

/// Add the credential and tenant headers `/tdata` requires.
///
/// Both or neither: the endpoint needs the bearer token *and* `X-Tenant-Id`,
/// and supplying only the token yields a confusing authorization error rather
/// than a missing-tenant one.
fn authorize(
    request: reqwest::RequestBuilder,
    token: Option<&str>,
    tenant: Option<&str>,
) -> reqwest::RequestBuilder {
    let request = match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    };
    match tenant {
        Some(tenant) => request.header("X-Tenant-Id", tenant),
        None => request,
    }
}

/// The names of the entity sets the service document advertises.
async fn fetch_entity_sets(
    client: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    tenant: Option<&str>,
) -> Result<Vec<String>> {
    let url = format!("{base}/tdata");
    // Unlike a single set, a refused service document is fatal: without it
    // there is nothing to audit.
    let body = match read_tdata(client, &url, token, tenant).await? {
        Read::Body(body) => body,
        Read::Forbidden => {
            anyhow::bail!(refusal(&url, reqwest::StatusCode::FORBIDDEN, token, tenant))
        }
    };
    Ok(body
        .get("value")
        .and_then(serde_json::Value::as_array)
        .map(|sets| {
            sets.iter()
                .filter_map(|set| set.get("name").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// One entity set's rows, or `None` if reading the set was refused.
async fn fetch_rows(
    client: &reqwest::Client,
    base: &str,
    set: &str,
    token: Option<&str>,
    tenant: Option<&str>,
) -> Result<Option<Vec<serde_json::Value>>> {
    let mut url = reqwest::Url::parse(&format!("{base}/tdata/{set}?$top={PAGE_LIMIT}"))?;
    let origin = url.origin();
    let mut visited = BTreeSet::new();
    let mut rows = Vec::new();
    loop {
        if !visited.insert(url.to_string()) {
            anyhow::bail!("repeated audit continuation for '{set}': {url}");
        }
        let body = match read_tdata(client, url.as_str(), token, tenant).await? {
            Read::Body(body) => body,
            Read::Forbidden if visited.len() == 1 => return Ok(None),
            Read::Forbidden => {
                anyhow::bail!("continuation for '{set}' refused the read; audit is incomplete")
            }
        };
        let page = body
            .get("value")
            .and_then(serde_json::Value::as_array)
            .with_context(|| format!("GET {url} has no collection `value` array"))?;
        rows.extend(page.iter().cloned());
        match body.get("@odata.nextLink") {
            None => return Ok(Some(rows)),
            Some(next) => {
                let next = next.as_str().context("invalid audit continuation link")?;
                let next = url.join(next)?;
                // Continuations retain authorization, so never send them to
                // another origin, even if the response advertises one.
                if next.origin() != origin {
                    anyhow::bail!("cross-origin audit continuation for '{set}'");
                }
                url = next;
            }
        }
    }
}

/// What a `/tdata` read produced.
///
/// A forbidden set is not a failed audit. The service document lists Temper's
/// own internal sets -- `AgentCredentials` among them -- and a token scoped for
/// an app's entities is rightly refused on those. Aborting on the first refusal
/// means the audit never reaches the sets it exists to check, which is exactly
/// how this behaved the first time it was pointed at a live server.
enum Read {
    Body(serde_json::Value),
    Forbidden,
}

/// Why a `/tdata` read was refused, phrased for the credentials actually sent.
///
/// The first version of this said "set TEMPER_TOKEN and TEMPER_TENANT" for
/// every 403, including the ones where both were set and correct. Blaming the
/// caller's configuration for a policy decision sends them to fix the one thing
/// that was not wrong -- the same failure the verification diagnostics in this
/// change exist to prevent, one layer down.
fn refusal(
    url: &str,
    status: reqwest::StatusCode,
    token: Option<&str>,
    tenant: Option<&str>,
) -> String {
    if status == reqwest::StatusCode::UNAUTHORIZED && token.is_some() {
        return format!(
            "GET {url} returned {status}; the bearer token was rejected -- verify or refresh TEMPER_TOKEN"
        );
    }
    match (token.is_some(), tenant.is_some()) {
        (true, true) => format!(
            "GET {url} returned {status}; access was refused -- verify the tenant scope and its Cedar read policy"
        ),
        (false, true) => {
            format!("GET {url} returned {status}; no bearer token was sent -- set TEMPER_TOKEN")
        }
        (true, false) => format!(
            "GET {url} returned {status}; no tenant was sent -- set TEMPER_TENANT or pass --tenant"
        ),
        (false, false) => format!(
            "GET {url} returned {status}; /tdata needs both a bearer token and a tenant -- set \
             TEMPER_TOKEN and TEMPER_TENANT (or pass --tenant)"
        ),
    }
}

async fn read_tdata(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    tenant: Option<&str>,
) -> Result<Read> {
    let response = authorize(client.get(url), token, tenant)
        .send()
        .await
        .with_context(|| format!("failed to GET {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable response body>".to_string());
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // Authentication failures invalidate the audit, unlike a forbidden
        // internal entity set. Preserve the status and identify token repair.
        anyhow::bail!(refusal(url, status, token, tenant));
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Ok(Read::Forbidden);
    }
    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}: {body}");
    }
    Ok(Read::Body(
        serde_json::from_str(&body).with_context(|| format!("{url} did not return JSON"))?,
    ))
}

/// Parse every `*.ioa.toml` in `specs_dir`, keyed by entity type name.
fn read_automata(specs_dir: &Path) -> Result<BTreeMap<String, Automaton>> {
    let mut automata = BTreeMap::new();
    if !specs_dir.is_dir() {
        anyhow::bail!("{} is not a directory", specs_dir.display());
    }
    for entry in std::fs::read_dir(specs_dir)
        .with_context(|| format!("failed to read {}", specs_dir.display()))?
    {
        let path = entry?.path();
        if !path.to_string_lossy().ends_with(".ioa.toml") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let automaton = parse_automaton(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        automata.insert(automaton.automaton.name.clone(), automaton);
    }
    Ok(automata)
}

#[cfg(test)]
#[path = "audit_test.rs"]
mod tests;
