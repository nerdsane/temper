//! TensorLake sandbox process + file door.
//!
//! Official APIs (https://docs.tensorlake.ai/sandboxes/commands,
//! https://docs.tensorlake.ai/sandboxes/file-operations):
//!
//! - `POST {url}/api/v1/processes` then poll `GET .../processes/{pid}`
//!   and `.../stdout` / `.../stderr`
//! - `GET`/`PUT` `{url}/api/v1/files?path=`
//!
//! Management traffic is authenticated. Never log the bearer value.
//! This module does not create or resume sandboxes.

use temper_wasm_sdk::prelude::*;

const SECRET_MARKER: &str = "{secret:";
const PROCESS_POLL_BUDGET: u8 = 32;

/// Which HTTP dialect tool_runner should speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxDoor {
    /// TensorLake process + file proxy. Used when `TEMPER_SANDBOX_NAME`
    /// is set or the URL host is `*.sandbox.tensorlake.ai`.
    TensorLake,
    /// E2B envd. Used when the name is empty and the host is E2B.
    E2B,
    /// Local `/v1/fs/file` + `/v1/processes/run`.
    Local,
}

/// Choose the connect door. Empty name keeps E2B/local. Name or TensorLake
/// host selects TensorLake. No create client.
pub fn sandbox_door(name: Option<&str>, sandbox_url: &str) -> SandboxDoor {
    if uses_tensorlake(name, sandbox_url) {
        SandboxDoor::TensorLake
    } else if is_e2b_host(sandbox_url) {
        SandboxDoor::E2B
    } else {
        SandboxDoor::Local
    }
}

/// True when the operator asked for the named TensorLake sandbox or the
/// stored URL is already a TensorLake proxy host.
pub fn uses_tensorlake(name: Option<&str>, sandbox_url: &str) -> bool {
    usable(name).is_some() || host_is_tensorlake(sandbox_url)
}

fn is_e2b_host(sandbox_url: &str) -> bool {
    let host = host_of(sandbox_url);
    host.ends_with("e2b.app") || host.ends_with("e2b.dev")
}

fn host_is_tensorlake(sandbox_url: &str) -> bool {
    let host = host_of(sandbox_url);
    host == "sandbox.tensorlake.ai" || host.ends_with(".sandbox.tensorlake.ai")
}

fn host_of(url: &str) -> &str {
    let after_scheme = match url.split_once("://") {
        Some((_, rest)) => rest,
        None => url,
    };
    let hostport = after_scheme.split('/').next().unwrap_or(after_scheme);
    hostport.split(':').next().unwrap_or(hostport)
}

fn usable(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    if value.is_empty() || value.contains(SECRET_MARKER) {
        None
    } else {
        Some(value)
    }
}

fn trim_base(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

/// `POST` target that starts a process.
pub fn process_start_url(sandbox_url: &str) -> String {
    format!("{}/api/v1/processes", trim_base(sandbox_url))
}

/// `GET` target that polls process status.
pub fn process_status_url(sandbox_url: &str, pid: &str) -> String {
    format!("{}/api/v1/processes/{pid}", trim_base(sandbox_url))
}

/// `GET` target for buffered stdout.
pub fn process_stdout_url(sandbox_url: &str, pid: &str) -> String {
    format!("{}/api/v1/processes/{pid}/stdout", trim_base(sandbox_url))
}

/// `GET` target for buffered stderr.
pub fn process_stderr_url(sandbox_url: &str, pid: &str) -> String {
    format!("{}/api/v1/processes/{pid}/stderr", trim_base(sandbox_url))
}

/// `GET`/`PUT`/`DELETE` target for one file.
pub fn file_url(sandbox_url: &str, path: &str) -> String {
    format!(
        "{}/api/v1/files?path={}",
        trim_base(sandbox_url),
        url_encode(path)
    )
}

/// `GET` target that lists a directory.
#[allow(dead_code)]
pub fn file_list_url(sandbox_url: &str, path: &str) -> String {
    format!(
        "{}/api/v1/files/list?path={}",
        trim_base(sandbox_url),
        url_encode(path)
    )
}

/// JSON body for `POST /api/v1/processes` (official fields only).
pub fn process_start_body(command: &str, args: &[&str], working_dir: &str) -> String {
    json!({
        "command": command,
        "args": args,
        "env": {},
        "working_dir": working_dir,
    })
    .to_string()
}

/// Bearer headers. Never log `api_key`.
pub fn tensorlake_auth_headers(api_key: &str) -> Result<Vec<(String, String)>, String> {
    let key = usable(Some(api_key)).ok_or_else(missing_key)?;
    Ok(vec![
        ("authorization".to_string(), format!("Bearer {key}")),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ])
}

/// File write uses octet-stream, still Bearer.
pub fn tensorlake_file_headers(api_key: &str) -> Result<Vec<(String, String)>, String> {
    let key = usable(Some(api_key)).ok_or_else(missing_key)?;
    Ok(vec![
        ("authorization".to_string(), format!("Bearer {key}")),
        (
            "content-type".to_string(),
            "application/octet-stream".to_string(),
        ),
    ])
}

fn missing_key() -> String {
    "tensorlake_api_key / TENSORLAKE_API_KEY is not set. Stock sandbox dsf \
     (dd comp) and overlay the key; this guest does not create TensorLake \
     sandboxes. Value is never printed."
        .to_string()
}

/// Resolve the API key from config. Does not log the value.
pub fn api_key_from_config(
    config: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    usable(config.get("tensorlake_api_key").map(String::as_str))
        .or_else(|| usable(config.get("TENSORLAKE_API_KEY").map(String::as_str)))
        .map(str::to_string)
        .ok_or_else(missing_key)
}

/// Named-sandbox name from config or entity fields.
pub fn sandbox_name_from_ctx(ctx: &Context) -> Option<String> {
    usable(ctx.config.get("temper_sandbox_name").map(String::as_str))
        .map(str::to_string)
        .or_else(|| {
            ctx.entity_state
                .get("fields")
                .and_then(|fields| fields.get("temper_sandbox_name"))
                .and_then(Value::as_str)
                .and_then(|value| usable(Some(value)).map(str::to_string))
        })
}

/// Parse `pid` from a start or status JSON body.
pub fn parse_pid(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    if let Some(n) = parsed.get("pid").and_then(Value::as_u64) {
        return Some(n.to_string());
    }
    parsed
        .get("pid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// True when the process is no longer running.
pub fn process_finished(body: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    match parsed.get("status").and_then(Value::as_str) {
        Some("running") => false,
        Some(_) => true,
        None => {
            parsed.get("exit_code").and_then(Value::as_i64).is_some()
                && parsed
                    .get("ended_at")
                    .map(|v| !v.is_null())
                    .unwrap_or(false)
        }
    }
}

fn exit_code(body: &str) -> i64 {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|parsed| parsed.get("exit_code").and_then(Value::as_i64))
        .unwrap_or(0)
}

fn output_text(body: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if let Some(text) = parsed.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(lines) = parsed.get("lines").and_then(Value::as_array) {
        return lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
    }
    body.to_string()
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Read a file through the TensorLake proxy. Does not call a create API.
pub fn read_file(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    api_key: &str,
) -> Result<String, String> {
    let url = file_url(sandbox_url, full_path);
    let headers = tensorlake_file_headers(api_key)?;
    ctx.log(
        "info",
        "tool_runner: TensorLake GET file (path in URL, no secrets)",
    );
    let resp = ctx.http_call("GET", &url, &headers, "")?;
    if resp.status == 200 {
        Ok(resp.body)
    } else {
        Err(format!("TensorLake read failed (HTTP {})", resp.status))
    }
}

/// Write a file through the TensorLake proxy.
pub fn write_file(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    content: &str,
    api_key: &str,
) -> Result<String, String> {
    let url = file_url(sandbox_url, full_path);
    let headers = tensorlake_file_headers(api_key)?;
    ctx.log(
        "info",
        "tool_runner: TensorLake PUT file (path in URL, no secrets)",
    );
    let resp = ctx.http_call("PUT", &url, &headers, content)?;
    if (200..300).contains(&resp.status) {
        Ok(format!("File written: {full_path}"))
    } else {
        Err(format!("TensorLake write failed (HTTP {})", resp.status))
    }
}

/// Run a shell command: start process, poll status, then read stdout/stderr.
pub fn run_bash(
    ctx: &Context,
    sandbox_url: &str,
    command: &str,
    workdir: &str,
    api_key: &str,
) -> Result<String, String> {
    let start_url = process_start_url(sandbox_url);
    let headers = tensorlake_auth_headers(api_key)?;
    let body = process_start_body("bash", &["-c", command], workdir);
    ctx.log(
        "info",
        &format!("tool_runner: TensorLake POST {start_url} (bearer not logged)"),
    );
    let started = ctx.http_call("POST", &start_url, &headers, &body)?;
    if !(200..300).contains(&started.status) {
        return Err(format!(
            "TensorLake process start failed (HTTP {})",
            started.status
        ));
    }
    let pid = parse_pid(&started.body)
        .ok_or_else(|| "TensorLake process start returned no pid (body not printed)".to_string())?;

    let mut status_body = started.body.clone();
    if !process_finished(&status_body) {
        let status_url = process_status_url(sandbox_url, &pid);
        let mut attempts = 0u8;
        while attempts < PROCESS_POLL_BUDGET && !process_finished(&status_body) {
            let polled = ctx.http_call("GET", &status_url, &headers, "")?;
            if !(200..300).contains(&polled.status) {
                return Err(format!(
                    "TensorLake process poll failed (HTTP {})",
                    polled.status
                ));
            }
            status_body = polled.body;
            attempts = attempts.saturating_add(1);
        }
        if !process_finished(&status_body) {
            return Err(
                "TensorLake process still running after poll budget. Resume is TensorLake's \
                 (`tl sbx resume dsf`); this guest does not create or resume sandboxes."
                    .to_string(),
            );
        }
    }

    let stdout = fetch_output(ctx, &process_stdout_url(sandbox_url, &pid), &headers)?;
    let stderr = fetch_output(ctx, &process_stderr_url(sandbox_url, &pid), &headers)?;
    Ok(combine_output(&stdout, &stderr, exit_code(&status_body)))
}

fn fetch_output(ctx: &Context, url: &str, headers: &[(String, String)]) -> Result<String, String> {
    let resp = ctx.http_call("GET", url, headers, "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "TensorLake output fetch failed (HTTP {})",
            resp.status
        ));
    }
    Ok(output_text(&resp.body))
}

fn combine_output(stdout: &str, stderr: &str, exit_code: i64) -> String {
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(stdout);
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("STDERR: ");
        output.push_str(stderr);
    }
    if exit_code != 0 {
        output.push_str(&format!("\n(exit code: {exit_code})"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    const DSF_NAME_URL: &str = "https://dsf.sandbox.tensorlake.ai";
    const DSF_ID_URL: &str = "https://053bmlgkq8a2zf3o4hcut.sandbox.tensorlake.ai";

    #[test]
    fn process_urls_use_official_proxy_paths() {
        assert_eq!(
            process_start_url(DSF_NAME_URL),
            "https://dsf.sandbox.tensorlake.ai/api/v1/processes"
        );
        assert_eq!(
            process_start_url(&format!("{DSF_ID_URL}/")),
            "https://053bmlgkq8a2zf3o4hcut.sandbox.tensorlake.ai/api/v1/processes"
        );
        assert_eq!(
            process_status_url(DSF_NAME_URL, "294"),
            "https://dsf.sandbox.tensorlake.ai/api/v1/processes/294"
        );
        assert_eq!(
            process_stdout_url(DSF_NAME_URL, "294"),
            "https://dsf.sandbox.tensorlake.ai/api/v1/processes/294/stdout"
        );
        assert_eq!(
            process_stderr_url(DSF_NAME_URL, "294"),
            "https://dsf.sandbox.tensorlake.ai/api/v1/processes/294/stderr"
        );
    }

    #[test]
    fn file_urls_use_official_proxy_paths() {
        assert_eq!(
            file_url(DSF_NAME_URL, "/workspace/data.csv"),
            "https://dsf.sandbox.tensorlake.ai/api/v1/files?path=/workspace/data.csv"
        );
        assert_eq!(
            file_list_url(DSF_NAME_URL, "/workspace"),
            "https://dsf.sandbox.tensorlake.ai/api/v1/files/list?path=/workspace"
        );
    }

    #[test]
    fn auth_headers_are_bearer_and_do_not_appear_in_urls() {
        let headers = tensorlake_auth_headers("unit-test-key").expect("key");
        assert_eq!(headers[0].0, "authorization");
        assert_eq!(headers[0].1, "Bearer unit-test-key");
        assert!(process_start_url(DSF_NAME_URL).contains("/api/v1/processes"));
        assert!(!process_start_url(DSF_NAME_URL).contains("unit-test-key"));
        assert!(tensorlake_auth_headers("").is_err());
        assert!(tensorlake_auth_headers("{secret:tensorlake_api_key}").is_err());
    }

    #[test]
    fn start_body_uses_official_fields() {
        let body = process_start_body("bash", &["-c", "echo hi"], "/workspace");
        let parsed: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(parsed["command"], "bash");
        assert_eq!(parsed["args"][0], "-c");
        assert_eq!(parsed["args"][1], "echo hi");
        assert_eq!(parsed["working_dir"], "/workspace");
        assert!(parsed.get("env").is_some());
    }

    #[test]
    fn name_or_tensorlake_host_selects_tensorlake_empty_name_keeps_e2b() {
        assert_eq!(
            sandbox_door(Some("dsf"), "https://example.invalid"),
            SandboxDoor::TensorLake
        );
        assert_eq!(sandbox_door(None, DSF_NAME_URL), SandboxDoor::TensorLake);
        assert_eq!(
            sandbox_door(Some(""), "https://49983-abc.e2b.app"),
            SandboxDoor::E2B
        );
        assert_eq!(
            sandbox_door(
                Some("{secret:temper_sandbox_name}"),
                "http://127.0.0.1:49983"
            ),
            SandboxDoor::Local
        );
        assert_eq!(
            sandbox_door(None, "http://127.0.0.1:49983"),
            SandboxDoor::Local
        );
    }

    #[test]
    fn parse_pid_and_finished_from_official_shapes() {
        assert_eq!(
            parse_pid(r#"{"pid":294,"status":"running"}"#).as_deref(),
            Some("294")
        );
        assert!(!process_finished(r#"{"pid":294,"status":"running"}"#));
        assert!(process_finished(
            r#"{"pid":305,"status":"exited","exit_code":1}"#
        ));
    }
}
