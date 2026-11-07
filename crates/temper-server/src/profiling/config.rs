//! Profiler settings, read from the environment on every use so a live
//! process can be switched without a restart.

/// Where profiles are uploaded and what goes with each upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Intake {
    /// The intake's full URL.
    pub(super) url: String,
    /// Extra request headers, for example a bearer token for a forwarder.
    pub(super) headers: Vec<(String, String)>,
}

pub(super) fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name) // determinism-ok: observability config, read outside simulation
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn flag(name: &str) -> bool {
    non_empty_env(name).is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
}

fn number(name: &str, default: u64) -> u64 {
    non_empty_env(name)
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// Whether the on-demand endpoints serve profiles.
pub(super) fn on_demand_enabled() -> bool {
    flag("TEMPER_PROFILING_ENABLED")
}

/// Whether an on-demand capture is also uploaded.
pub(super) fn auto_upload_enabled() -> bool {
    flag("TEMPER_PROFILING_AUTO_UPLOAD")
}

/// Whether the continuous capture-and-upload loop runs.
pub(super) fn continuous_enabled() -> bool {
    flag("TEMPER_PROFILING_CONTINUOUS")
}

/// Longest on-demand window: a caller cannot hold a capture for ten minutes.
pub(super) fn max_window_seconds() -> u64 {
    number("TEMPER_PROFILING_MAX_SECONDS", 120).clamp(5, 600)
}

pub(super) fn continuous_interval_seconds() -> u64 {
    number("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS", 300).clamp(60, 3_600)
}

pub(super) fn continuous_window_seconds() -> u64 {
    number("TEMPER_PROFILING_CONTINUOUS_SECONDS", 30).clamp(5, max_window_seconds())
}

/// Sampling rate in Hz. pprof's timer takes an `i32`; the clamp keeps it there.
pub(super) fn continuous_frequency() -> i32 {
    number("TEMPER_PROFILING_CONTINUOUS_FREQUENCY", 100).clamp(10, 500) as i32
}

/// The configured intake, or the local Datadog Agent's profile endpoint.
pub(super) fn intake() -> Intake {
    let url = non_empty_env("TEMPER_PROFILING_INTAKE_URL").unwrap_or_else(|| {
        let host = non_empty_env("DD_AGENT_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = non_empty_env("DD_TRACE_AGENT_PORT").unwrap_or_else(|| "8126".to_string());
        format!("http://{host}:{port}/profiling/v1/input")
    });
    let mut headers: Vec<(String, String)> = non_empty_env("TEMPER_PROFILING_INTAKE_HEADERS")
        .map(|raw| {
            temper_observe::otel::parse_otlp_headers(&raw)
                .into_iter()
                .collect()
        })
        .unwrap_or_default();
    headers.sort();
    Intake { url, headers }
}

/// The service profiles are filed under: `DD_SERVICE`, else the OTLP service
/// name, else `temper`.
pub(super) fn service_name() -> String {
    non_empty_env("DD_SERVICE")
        .or_else(|| non_empty_env("OTEL_SERVICE_NAME"))
        .unwrap_or_else(|| "temper".to_string())
}

/// `DD_ENV`. Without it the upload names no environment and the receiving
/// Agent or forwarder supplies its own default.
pub(super) fn deployment_environment() -> Option<String> {
    non_empty_env("DD_ENV")
}

/// `BUILD_VERSION`, else `DD_VERSION`; none when neither is set.
pub(super) fn service_version() -> Option<String> {
    non_empty_env("BUILD_VERSION").or_else(|| non_empty_env("DD_VERSION"))
}

/// `DD_TAGS` as `key:value` tags; Datadog accepts commas or spaces between them.
pub(super) fn extra_tags() -> Vec<String> {
    non_empty_env("DD_TAGS")
        .map(|raw| {
            raw.split([',', ' '])
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}
