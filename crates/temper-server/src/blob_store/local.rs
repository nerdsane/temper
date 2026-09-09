use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::Instrument as _;

use crate::blob_transport_observability::{
    BlobTransportFinish, blob_transport_span, finish_blob_transport,
};

pub(super) async fn put_local_blob_observed(
    root: &Path,
    key: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<(), String> {
    put_local_blob_mode_observed(root, key, body, operation, false).await
}

pub(super) async fn put_local_blob_replace_observed(
    root: &Path,
    key: &str,
    body: &[u8],
    operation: &'static str,
) -> Result<(), String> {
    put_local_blob_mode_observed(root, key, body, operation, true).await
}

async fn put_local_blob_mode_observed(
    root: &Path,
    key: &str,
    body: &[u8],
    operation: &'static str,
    replace_existing: bool,
) -> Result<(), String> {
    let request_bytes = body.len() as u64;
    let started_at = Instant::now(); // determinism-ok: production blob transport metric only
    let span = blob_transport_span(operation, "local_fs", request_bytes);
    let result = put_local_blob_atomic(root, key, body, replace_existing)
        .instrument(span.clone())
        .await;
    finish_blob_transport(BlobTransportFinish {
        started_at,
        span: &span,
        operation,
        backend: "local_fs",
        outcome: if result.is_ok() { "ok" } else { "error" },
        status: None,
        request_bytes,
        response_bytes: 0,
    });
    result
}

pub(super) async fn get_local_blob_observed(
    root: &Path,
    key: &str,
) -> Result<Option<Vec<u8>>, String> {
    let started_at = Instant::now(); // determinism-ok: production blob transport metric only
    let span = blob_transport_span("get", "local_fs", 0);
    let result = get_local_blob(root, key).instrument(span.clone()).await;
    let (outcome, response_bytes) = match &result {
        Ok(Some(bytes)) => ("ok", bytes.len() as u64),
        Ok(None) => ("not_found", 0),
        Err(_) => ("error", 0),
    };
    finish_blob_transport(BlobTransportFinish {
        started_at,
        span: &span,
        operation: "get",
        backend: "local_fs",
        outcome,
        status: None,
        request_bytes: 0,
        response_bytes,
    });
    result
}

pub(super) async fn get_local_blob_bounded_observed(
    root: &Path,
    key: &str,
    max_bytes: usize,
) -> Result<super::BlobReadBounded, String> {
    let started_at = Instant::now(); // determinism-ok: production blob transport metric only
    let span = blob_transport_span("get_bounded", "local_fs", 0);
    let result = get_local_blob_bounded(root, key, max_bytes)
        .instrument(span.clone())
        .await;
    let (outcome, response_bytes) = match &result {
        Ok(super::BlobReadBounded::Found(bytes)) => ("ok", bytes.len() as u64),
        Ok(super::BlobReadBounded::Missing) => ("not_found", 0),
        Ok(super::BlobReadBounded::TooLarge { .. }) => ("too_large", 0),
        Err(_) => ("error", 0),
    };
    finish_blob_transport(BlobTransportFinish {
        started_at,
        span: &span,
        operation: "get_bounded",
        backend: "local_fs",
        outcome,
        status: None,
        request_bytes: 0,
        response_bytes,
    });
    result
}

pub(super) fn local_blob_path(root: &Path, key: &str) -> Result<PathBuf, String> {
    let mut path = root.to_path_buf();
    let mut saw_component = false;
    for component in key.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains('\\')
            || component.starts_with(std::path::MAIN_SEPARATOR)
        {
            return Err(format!("invalid blob key '{key}'"));
        }
        saw_component = true;
        path.push(component);
    }
    if !saw_component {
        return Err("invalid empty blob key".to_string());
    }
    Ok(path)
}

async fn put_local_blob_atomic(
    root: &Path,
    key: &str,
    body: &[u8],
    replace_existing: bool,
) -> Result<(), String> {
    let path = local_blob_path(root, key)?;
    if !replace_existing
        && tokio::fs::try_exists(&path)
            .await
            .map_err(|error| format!("failed to check local blob '{}': {error}", path.display()))?
    {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("local blob '{}' has no parent", path.display()))?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        format!(
            "failed to create local blob dir '{}': {error}",
            parent.display()
        )
    })?;
    let parent = parent.to_path_buf();
    // determinism-ok: production object-store filesystem boundary
    let staged = tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix("object-put-")
            .tempfile_in(parent)
    })
    .await
    .map_err(|error| format!("local blob staging task failed: {error}"))?
    .map_err(|error| format!("failed to create local blob staging file: {error}"))?;
    let (file, staged_path) = staged.into_parts();
    let mut file = tokio::fs::File::from_std(file);
    file.write_all(body)
        .await
        .map_err(|error| format!("failed to write local blob '{}': {error}", path.display()))?;
    file.flush().await.map_err(|error| error.to_string())?;
    file.sync_data().await.map_err(|error| error.to_string())?;
    drop(file);
    if replace_existing {
        tokio::fs::rename(staged_path.to_path_buf(), &path)
            .await
            .map_err(|error| format!("failed to replace local blob '{}': {error}", path.display()))
    } else {
        match tokio::fs::hard_link(staged_path.to_path_buf(), &path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(format!(
                "failed to publish local blob '{}': {error}",
                path.display()
            )),
        }
    }
}

async fn get_local_blob(root: &Path, key: &str) -> Result<Option<Vec<u8>>, String> {
    let path = local_blob_path(root, key)?;
    if !tokio::fs::try_exists(&path)
        .await
        .map_err(|error| format!("failed to check local blob '{}': {error}", path.display()))?
    {
        return Ok(None);
    }
    tokio::fs::read(&path)
        .await
        .map(Some)
        .map_err(|error| format!("failed to read local blob '{}': {error}", path.display()))
}

async fn get_local_blob_bounded(
    root: &Path,
    key: &str,
    max_bytes: usize,
) -> Result<super::BlobReadBounded, String> {
    let path = local_blob_path(root, key)?;
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(super::BlobReadBounded::Missing);
        }
        Err(error) => {
            return Err(format!(
                "failed to open local blob '{}': {error}",
                path.display()
            ));
        }
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|error| format!("failed to stat local blob '{}': {error}", path.display()))?;
    if metadata.len() > max_bytes as u64 {
        return Ok(super::BlobReadBounded::TooLarge {
            actual_bytes: Some(metadata.len()),
        });
    }

    let bounded_len = max_bytes
        .checked_add(1)
        .ok_or_else(|| "bounded blob read limit overflowed usize".to_string())?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(bounded_len as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("failed to read local blob '{}': {error}", path.display()))?;
    if bytes.len() > max_bytes {
        return Ok(super::BlobReadBounded::TooLarge {
            actual_bytes: Some(bytes.len() as u64),
        });
    }
    Ok(super::BlobReadBounded::Found(bytes))
}

#[cfg(test)]
mod tests {
    use super::super::BlobStore;

    #[tokio::test]
    async fn local_blob_store_round_trips_without_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::local_fs(dir.path());
        store
            .put_if_absent("wasm-modules/hash-a", b"hello", None)
            .await
            .expect("put");
        assert_eq!(
            store.get("wasm-modules/hash-a").await.unwrap().unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn local_blob_store_rejects_path_traversal_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = BlobStore::local_fs(dir.path())
            .put_if_absent("../escape", b"nope", None)
            .await
            .expect_err("path traversal rejected");
        assert!(error.contains("invalid blob key"));
    }

    #[tokio::test]
    async fn content_addressed_write_repairs_existing_local_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::local_fs(dir.path());
        store
            .put_if_absent("field-overflow/sha256/value.json", b"corrupt", None)
            .await
            .expect("seed corrupt object");

        store
            .put_content_addressed("field-overflow/sha256/value.json", b"canonical", None)
            .await
            .expect("repair object");

        assert_eq!(
            store
                .get("field-overflow/sha256/value.json")
                .await
                .expect("read object"),
            Some(b"canonical".to_vec())
        );
    }
}
