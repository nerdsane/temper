//! Upload a profile in the multipart shape Datadog's profilers use: an
//! `event` part (`event.json`) and the profile itself as `profile.pprof`.
//! The same request works against a Datadog Agent and against a forwarder in
//! front of Datadog's profile intake.

use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;

use super::config::{self, Intake};
use super::metrics::{profile_attrs, profile_stage_attrs, profiling_metrics};

/// The attachment name Datadog's intake reads the profile from.
pub(super) const PROFILE_ATTACHMENT: &str = "profile.pprof";

/// Datadog's family for compiled-language profiles (its own native profiler).
const FAMILY: &str = "native";

/// A finished capture, ready to upload.
pub(super) struct Captured {
    pub(super) pprof: Vec<u8>,
    /// `cpu`, `wall` or `heap`; tags the upload as `profile.component`.
    pub(super) profile_type: &'static str,
    /// `continuous` or `on_demand`.
    pub(super) mode: &'static str,
    pub(super) started_at: DateTime<Utc>,
    pub(super) ended_at: DateTime<Utc>,
}

pub(super) fn profile_tags(profile_type: &str, mode: &str) -> Vec<String> {
    let mut tags = vec![format!("service:{}", config::service_name())];
    tags.extend(config::deployment_environment().map(|env| format!("env:{env}")));
    tags.extend(config::service_version().map(|version| format!("version:{version}")));
    tags.extend([
        format!("runtime-id:{}", temper_observe::otel::runtime_id()),
        "runtime:rust".to_string(),
        format!("profile.component:{profile_type}"),
        format!("profile.mode:{mode}"),
    ]);
    tags.extend(config::extra_tags());
    tags
}

pub(super) fn event_json(captured: &Captured) -> serde_json::Value {
    let activation = if config::continuous_enabled() {
        "auto"
    } else {
        "manual"
    };
    json!({
        "version": "4",
        "family": FAMILY,
        "start": captured.started_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "end": captured.ended_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "attachments": [PROFILE_ATTACHMENT],
        "tags_profiler": profile_tags(captured.profile_type, captured.mode).join(","),
        "info": {
            "profiler": {
                "activation": activation,
                "ssi": { "mechanism": "none" },
                "settings": {
                    "profile_type": captured.profile_type,
                    "profile_source": "temper"
                }
            }
        }
    })
}

/// Send `captured` to `intake`. Failures are logged and counted, never
/// raised: a slow or absent intake must not hold the server up.
pub(super) async fn upload(captured: Captured, intake: Intake) {
    let (profile_type, mode) = (captured.profile_type, captured.mode);
    let stage_failed = |stage: &str| {
        profiling_metrics()
            .upload_errors
            .add(1, &profile_stage_attrs(profile_type, mode, stage));
    };
    let event = match serde_json::to_vec(&event_json(&captured)) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = %e, "profile upload: event.json did not encode");
            return stage_failed("event_encode");
        }
    };
    let form = reqwest::multipart::Form::new()
        .part(
            PROFILE_ATTACHMENT,
            reqwest::multipart::Part::bytes(captured.pprof)
                .file_name(PROFILE_ATTACHMENT)
                .mime_str("application/octet-stream")
                .expect("static profile MIME type is valid"),
        )
        .part(
            "event",
            reqwest::multipart::Part::bytes(event)
                .file_name("event.json")
                .mime_str("application/json")
                .expect("static event MIME type is valid"),
        );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(error = %e, "profile upload: HTTP client did not build");
            return stage_failed("client_build");
        }
    };
    match send(&client, &intake, form).await {
        Ok(status) if status.is_success() => {
            tracing::debug!(url = %intake.url, profile_type, "profile uploaded");
            profiling_metrics()
                .profiles_uploaded
                .add(1, &profile_attrs(profile_type, mode));
        }
        Ok(status) => {
            tracing::warn!(url = %intake.url, %status, profile_type, "profile upload refused");
            stage_failed("non_success_status");
        }
        Err(e) => {
            tracing::warn!(url = %intake.url, error = %e, profile_type, "profile upload failed");
            stage_failed("request_error");
        }
    }
}

async fn send(
    client: &reqwest::Client,
    intake: &Intake,
    form: reqwest::multipart::Form,
) -> Result<reqwest::StatusCode, reqwest::Error> {
    let mut request = client.post(&intake.url).multipart(form);
    for (name, value) in &intake.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    Ok(request.send().await?.status())
}
