use super::*;
use crate::McpConfig;
use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct LocalFile(PathBuf);
impl LocalFile {
    fn new(bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("temper-upload-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
    fn args(&self, bytes: &[u8]) -> Value {
        json!({"tenant":"fixture","file_id":"fl-example","local_path":self.0,"content_type":"image/png","expected_sha256":format!("{:x}",Sha256::digest(bytes))})
    }
}
impl Drop for LocalFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

type Captured = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;
async fn server(
    status: StatusCode,
    body: &'static str,
) -> (RuntimeContext, Captured, tokio::task::JoinHandle<()>) {
    let captures: Captured = Arc::new(Mutex::new(Vec::new()));
    let seen = captures.clone();
    let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, bytes: Bytes| {
        let seen = seen.clone();
        async move {
            seen.lock()
                .unwrap()
                .push((uri.to_string(), headers, bytes.to_vec()));
            (
                status,
                [("location", "http://127.0.0.1:1/never-follow")],
                body,
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_url: Some(format!("http://{address}/native/prefix")),
        temper_port: None,
        api_key: Some("fixture-key".into()),
        agent_id: None,
        agent_type: None,
        session_id: Some("fixture-session".into()),
    })
    .unwrap();
    (ctx, captures, handle)
}

#[tokio::test]
async fn exact_bytes_same_auth_and_hash_receipt() {
    let bytes = b"\x89PNG\r\n\x00\xffreal-fixture";
    let file = LocalFile::new(bytes);
    let (ctx, seen, handle) = server(StatusCode::NO_CONTENT, "").await;
    let (result, denials) = upload_file_bytes(&ctx, &file.args(bytes)).await;
    let receipt: Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert!(denials.is_empty());
    assert_eq!(receipt["size_bytes"], bytes.len());
    assert_eq!(receipt["sha256"], format!("{:x}", Sha256::digest(bytes)));
    assert_eq!(receipt["locked"], false);
    let captured = seen.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].0.ends_with("/tdata/Files('fl-example')/$value"));
    assert_eq!(captured[0].1["authorization"], "Bearer fixture-key");
    assert_eq!(captured[0].1["x-session-id"], "fixture-session");
    assert_eq!(captured[0].1["x-tenant-id"], "fixture");
    assert_eq!(captured[0].1["content-type"], "image/png");
    assert_eq!(captured[0].2, bytes);
    handle.abort();
}

#[tokio::test]
async fn mismatch_and_invalid_route_never_send() {
    let file = LocalFile::new(b"actual");
    let (ctx, seen, handle) = server(StatusCode::NO_CONTENT, "").await;
    assert!(
        upload_file_bytes(&ctx, &file.args(b"wrong"))
            .await
            .0
            .is_err()
    );
    for (field, value) in [
        ("file_id", "../Secrets('x')"),
        ("tenant", "bad\r\nAuthorization: other"),
        ("content_type", "bad\nheader"),
        ("local_path", "relative.png"),
    ] {
        let mut args = file.args(b"actual");
        args[field] = json!(value);
        assert!(upload_file_bytes(&ctx, &args).await.0.is_err());
    }
    assert!(seen.lock().unwrap().is_empty());
    handle.abort();
}

#[tokio::test]
async fn denial_preserved_and_redirect_not_followed() {
    let file = LocalFile::new(b"actual");
    let denial = r#"{"status":"authorization_denied","decision_id":"PD-fixture","reason":"File update forbidden"}"#;
    for (status, body) in [
        (StatusCode::FORBIDDEN, denial),
        (StatusCode::TEMPORARY_REDIRECT, "redirect denied"),
    ] {
        let (ctx, seen, handle) = server(status, body).await;
        let (result, denials) = upload_file_bytes(&ctx, &file.args(b"actual")).await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains(body));
        assert!(error.contains(&status.as_u16().to_string()));
        assert_eq!(seen.lock().unwrap().len(), 1);
        if status == StatusCode::FORBIDDEN {
            assert_eq!(denials.len(), 1);
            assert_eq!(denials[0].decision_id, "PD-fixture");
        } else {
            assert!(denials.is_empty());
        }
        handle.abort();
    }
}

#[test]
fn bounded_reads_missing_path_and_nonregular_file_reject() {
    let file = LocalFile::new(b"x");
    let digest = format!("{:x}", Sha256::digest(b"x"));
    assert!(read_bytes(file.0.parent().unwrap().to_str().unwrap(), &digest).is_err());
    assert!(read_bytes(file.0.with_extension("missing").to_str().unwrap(), &digest).is_err());
    std::fs::File::options()
        .write(true)
        .open(&file.0)
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    assert!(read_bytes(file.0.to_str().unwrap(), &digest).is_err());
    assert!(
        read_bounded(std::io::repeat(0), &digest)
            .unwrap_err()
            .to_string()
            .contains("exceeded 32 MiB")
    );
}

#[tokio::test]
async fn tool_is_discoverable_and_dispatches_without_execute() {
    let file = LocalFile::new(b"actual");
    let (mut ctx, _, handle) = server(StatusCode::NO_CONTENT, "").await;
    let listed = crate::protocol::dispatch_json_value(
        &mut ctx,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await
    .unwrap();
    assert!(
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "upload_file_bytes")
    );
    let result=crate::protocol::dispatch_json_value(&mut ctx,json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"upload_file_bytes","arguments":file.args(b"actual")}})).await.unwrap();
    assert_eq!(result["result"]["isError"], false);
    handle.abort();
}

#[cfg(unix)]
#[test]
fn fifo_is_rejected_without_waiting_for_a_writer() {
    let file = LocalFile::new(b"placeholder");
    std::fs::remove_file(&file.0).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&file.0)
            .status()
            .unwrap()
            .success()
    );
    let path = file.0.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = read_bytes(path.to_str().unwrap(), &"0".repeat(64));
        let _ = sender.send(result);
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("FIFO open blocked");
    assert!(result.unwrap_err().to_string().contains("regular file"));
}

#[tokio::test]
async fn oversized_success_response_is_not_a_success_receipt() {
    let file = LocalFile::new(b"actual");
    let body = Box::leak("x".repeat(MAX_RESPONSE_BYTES + 1).into_boxed_str());
    let (ctx, _, handle) = server(StatusCode::OK, body).await;
    let (result, _) = upload_file_bytes(&ctx, &file.args(b"actual")).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("response truncated at 64 KiB"));
    assert!(error.len() < MAX_RESPONSE_BYTES + 200);
    handle.abort();
}
