//! The local sandbox script `temper serve` starts: it answers only requests
//! that carry the token from its environment, and does not start without one.
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-sandbox-token";

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../os-apps/temper-agent/sandbox/local_sandbox.py")
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free loopback port")
}

fn sandbox_command(port: u16, workdir: &Path) -> Command {
    let mut command = Command::new("python3");
    command
        .arg(script())
        .args(["--port", &port.to_string(), "--workdir"])
        .arg(workdir)
        .args(["--token-env", "TEMPER_SANDBOX_TOKEN"])
        .env_remove("TEMPER_SANDBOX_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Wait until the sandbox accepts connections on `port`.
fn wait_until_listening(process: &mut Process, port: u16) -> SocketAddr {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(address).is_err() {
        if let Some(status) = process.0.try_wait().expect("read sandbox status") {
            panic!("sandbox exited with {status} before listening");
        }
        assert!(Instant::now() < deadline, "sandbox did not start listening");
        thread::sleep(Duration::from_millis(50));
    }
    address
}

/// Start the sandbox with the token and wait until it accepts connections.
fn start_sandbox(workdir: &Path) -> (Process, SocketAddr) {
    let port = free_port();
    let mut command = sandbox_command(port, workdir);
    command.env("TEMPER_SANDBOX_TOKEN", TOKEN);
    let mut process = Process(command.spawn().expect("start python3 sandbox"));
    let address = wait_until_listening(&mut process, port);
    (process, address)
}

/// Send one HTTP/1.0 request and return the status code and body.
fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).expect("connect to sandbox");
    let auth_line = auth
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n{auth_line}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .expect("write request head");
    stream
        .write_all(body.as_bytes())
        .expect("write request body");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code");
    let body = response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    (status, body)
}

fn run_body(marker: &Path) -> String {
    serde_json::json!({ "command": format!("touch {}", marker.display()) }).to_string()
}

#[test]
fn the_sandbox_refuses_requests_without_its_token() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (_sandbox, address) = start_sandbox(dir.path());
    let marker = dir.path().join("ran.txt");
    let secret_file = dir.path().join("kept.txt");
    std::fs::write(&secret_file, "kept").expect("write file");

    for auth in [None, Some("Bearer wrong-token"), Some(TOKEN)] {
        let (status, _) = request(
            address,
            "POST",
            "/v1/processes/run",
            auth,
            &run_body(&marker),
        );
        assert_eq!(status, 401, "run with {auth:?}");
        let path = format!("/v1/fs/file?path={}", secret_file.display());
        let (status, body) = request(address, "GET", &path, auth, "");
        assert_eq!(status, 401, "read with {auth:?}");
        assert!(
            !body.contains("kept"),
            "read with {auth:?} returned the file"
        );
        let (status, _) = request(address, "PUT", "/v1/fs/file?path=written.txt", auth, "x");
        assert_eq!(status, 401, "write with {auth:?}");
        let (status, _) = request(address, "GET", "/health", auth, "");
        assert_eq!(status, 401, "health with {auth:?}");
    }
    assert!(!marker.exists(), "a command ran without the token");
    assert!(
        !dir.path().join("written.txt").exists(),
        "a file was written without the token"
    );
}

#[test]
fn the_sandbox_serves_requests_with_its_token() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (_sandbox, address) = start_sandbox(dir.path());
    let auth = Some(format!("Bearer {TOKEN}"));
    let marker = dir.path().join("ran.txt");

    let (status, body) = request(
        address,
        "POST",
        "/v1/processes/run",
        auth.as_deref(),
        &run_body(&marker),
    );
    assert_eq!(status, 200, "{body}");
    assert!(marker.exists(), "the command did not run");
    let (status, _) = request(
        address,
        "PUT",
        "/v1/fs/file?path=note.txt",
        auth.as_deref(),
        "hello",
    );
    assert_eq!(status, 200);
    let (status, body) = request(
        address,
        "GET",
        "/v1/fs/file?path=note.txt",
        auth.as_deref(),
        "",
    );
    assert_eq!((status, body.as_str()), (200, "hello"));
}

#[test]
fn the_sandbox_does_not_start_without_a_token() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let port = free_port();
    let output = sandbox_command(port, dir.path())
        .output()
        .expect("run python3 sandbox");
    assert!(
        !output.status.success(),
        "the sandbox started without a token"
    );
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
        "something listens on the sandbox port"
    );
}

#[test]
fn commands_run_by_the_sandbox_do_not_see_its_token() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (_sandbox, address) = start_sandbox(dir.path());
    let auth = format!("Bearer {TOKEN}");
    let command = serde_json::json!({ "command": "printenv TEMPER_SANDBOX_TOKEN; env" });

    let (status, body) = request(
        address,
        "POST",
        "/v1/processes/run",
        Some(&auth),
        &command.to_string(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains(TOKEN),
        "a command's output shows the token: {body}"
    );
}

/// Start the sandbox as serve does: its stdin is a pipe the caller holds.
fn start_sandbox_on_a_pipe(workdir: &Path) -> (Process, SocketAddr) {
    let port = free_port();
    let mut command = sandbox_command(port, workdir);
    command
        .arg("--exit-with-stdin")
        .env("TEMPER_SANDBOX_TOKEN", TOKEN)
        .stdin(Stdio::piped());
    let mut process = Process(command.spawn().expect("start python3 sandbox"));
    let address = wait_until_listening(&mut process, port);
    (process, address)
}

#[test]
fn commands_get_a_closed_stdin_while_the_sandbox_holds_its_pipe() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (_sandbox, address) = start_sandbox_on_a_pipe(dir.path());
    let auth = format!("Bearer {TOKEN}");
    let command = serde_json::json!({ "command": "cat && echo stdin-closed" });

    let (status, body) = request(
        address,
        "POST",
        "/v1/processes/run",
        Some(&auth),
        &command.to_string(),
    );
    assert_eq!(status, 200, "{body}");
    let result: serde_json::Value = serde_json::from_str(&body).expect("JSON result");
    assert_eq!(result["exit_code"], 0, "{body}");
    assert_eq!(result["stdout"], "stdin-closed\n", "{body}");
}

#[test]
fn the_sandbox_stops_when_its_starter_closes_the_pipe() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (mut process, address) = start_sandbox_on_a_pipe(dir.path());

    drop(process.0.stdin.take());

    let deadline = Instant::now() + Duration::from_secs(10);
    while process.0.try_wait().expect("read sandbox status").is_none() {
        assert!(
            Instant::now() < deadline,
            "the sandbox kept running after its pipe closed"
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        TcpStream::connect(address).is_err(),
        "the sandbox port is still taken"
    );
}
