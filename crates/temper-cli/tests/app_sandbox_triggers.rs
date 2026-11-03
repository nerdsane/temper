//! App triggers that run tools in a sandbox reach the sandbox `temper serve`
//! starts, with its token.
//!
//! The local sandbox answers only requests that carry the token generated at
//! its start, and the WASM host adds that token only to requests for the
//! sandbox's own origin. A trigger that names its own sandbox address gets no
//! token, so the sandbox there refuses it. This walks every shipped app spec,
//! resolves each trigger's `sandbox_url` the way the server resolves trigger
//! config, and sends a request there through the host the server builds, to
//! the real `local_sandbox.py`.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use temper_server::secrets::{SecretsVault, resolve_secret_templates};
use temper_wasm::{LocalSandboxAccess, ProductionWasmHost, WasmHost};

/// The token `temper serve` gave the sandbox it started.
const SERVE_TOKEN: &str = "serve-sandbox-token";
/// The token of a sandbox someone started by hand, which serve does not know.
const HAND_TOKEN: &str = "hand-started-sandbox-token";
/// The trigger config key that names the sandbox.
const SANDBOX_URL: &str = "sandbox_url";
/// The app directories whose specs ship with Temper.
const APP_DIRS: &[&str] = &["os-apps", "reference-apps"];

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn checkout() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free loopback port")
}

/// Start `local_sandbox.py` on `port` with `token` and wait until it listens.
fn start_sandbox(port: u16, token: &str, workdir: &Path) -> Process {
    let mut process = Process(
        Command::new("python3")
            .arg(checkout().join("os-apps/temper-agent/sandbox/local_sandbox.py"))
            .args(["--port", &port.to_string(), "--workdir"])
            .arg(workdir)
            .args(["--token-env", "TEMPER_SANDBOX_TOKEN"])
            .env("TEMPER_SANDBOX_TOKEN", token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start python3 sandbox"),
    );
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(address).is_err() {
        if let Some(status) = process.0.try_wait().expect("read sandbox status") {
            panic!("sandbox on port {port} exited with {status} before listening");
        }
        assert!(
            Instant::now() < deadline,
            "sandbox on port {port} did not listen"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    process
}

/// Every `.ioa.toml` under `dir` that mentions the sandbox key.
fn specs_naming_a_sandbox(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("{}: cannot read directory: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            specs_naming_a_sandbox(&path, found);
        } else if path.to_string_lossy().ends_with(".ioa.toml") {
            let source = std::fs::read_to_string(&path).expect("read spec");
            if source.contains(SANDBOX_URL) {
                found.push((path, source));
            }
        }
    }
}

/// Each trigger that names a sandbox: its spec, its name and its config.
fn sandbox_triggers() -> Vec<(String, String, BTreeMap<String, String>)> {
    let mut specs = Vec::new();
    for dir in APP_DIRS {
        specs_naming_a_sandbox(&checkout().join(dir), &mut specs);
    }
    specs.sort();
    let mut triggers = Vec::new();
    for (path, source) in specs {
        let automaton = temper_spec::automaton::parse_automaton(&source)
            .unwrap_or_else(|err| panic!("{} does not parse: {err}", path.display()));
        let spec = path
            .strip_prefix(checkout())
            .unwrap_or(&path)
            .display()
            .to_string();
        for integration in automaton.integrations {
            if integration.config.contains_key(SANDBOX_URL) {
                triggers.push((spec.clone(), integration.name, integration.config));
            }
        }
    }
    triggers
}

#[tokio::test(flavor = "multi_thread")]
async fn every_app_trigger_that_names_a_sandbox_reaches_the_served_sandbox_with_its_token() {
    let workdir = tempfile::tempdir().expect("temporary directory");
    // What `temper serve` does with no SANDBOX_URL: start the sandbox on a
    // port of its own, publish its URL as the `sandbox_url` platform secret,
    // and give the WASM host its token.
    let port = free_port();
    let _served = start_sandbox(port, SERVE_TOKEN, workdir.path());
    let served_url = format!("http://127.0.0.1:{port}");
    let vault = SecretsVault::new(&[7; 32]);
    vault.cache_platform_secret(SANDBOX_URL, served_url.clone());
    let host = ProductionWasmHost::new(BTreeMap::new()).with_local_sandbox(Some(
        LocalSandboxAccess::new(&served_url, SERVE_TOKEN.to_string()).expect("sandbox access"),
    ));

    let triggers = sandbox_triggers();
    assert!(!triggers.is_empty(), "no app trigger names a sandbox");
    let mut hand_started = BTreeMap::new();
    for (spec, trigger, config) in triggers {
        let url = resolve_secret_templates(&config, &vault, "default")[SANDBOX_URL].clone();
        let parsed = reqwest::Url::parse(&url).unwrap_or_else(|err| {
            panic!("{spec} {trigger}: sandbox_url {url} is not a URL: {err}")
        });
        let named_port = parsed.port_or_known_default().expect("sandbox port");
        if named_port != port && !hand_started.contains_key(&named_port) {
            // A trigger that names an address of its own expects a sandbox
            // someone started there by hand, which has its own token.
            hand_started.insert(
                named_port,
                start_sandbox(named_port, HAND_TOKEN, workdir.path()),
            );
        }

        let (status, body) = host
            .http_call("GET", &format!("{url}/health"), &[], "")
            .await
            .unwrap_or_else(|err| panic!("{spec} {trigger}: GET {url}/health failed: {err}"));
        assert_eq!(
            status, 200,
            "{spec} {trigger}: GET {url}/health answered {status} {body}"
        );
    }
}
