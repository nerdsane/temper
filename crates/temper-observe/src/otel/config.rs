//! Environment resolution helpers for OTEL setup.

use std::collections::HashMap;

/// Default OTLP endpoint for Logfire.
pub(super) const LOGFIRE_ENDPOINT: &str = "https://logfire-us.pydantic.dev";

#[derive(Clone, Copy, Debug)]
pub(super) enum EndpointSource {
    OtlpEndpoint,
    OtlpExporterEndpoint,
    LogfireToken,
}

impl EndpointSource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::OtlpEndpoint => "OTLP_ENDPOINT",
            Self::OtlpExporterEndpoint => "OTEL_EXPORTER_OTLP_ENDPOINT",
            Self::LogfireToken => "LOGFIRE_TOKEN",
        }
    }
}

#[derive(Debug)]
pub(super) struct ResolvedOtelConfig {
    pub(super) endpoint: String,
    pub(super) endpoint_source: EndpointSource,
    pub(super) logfire_token: Option<String>,
}

pub(super) fn read_non_empty_env(var_name: &str) -> Option<String> {
    std::env::var(var_name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(super) fn parse_otlp_headers(raw: &str) -> HashMap<String, String> {
    raw.split(',')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

pub(super) fn resolve_deployment_environment() -> Option<String> {
    if let Some(environment) = read_non_empty_env("DD_ENV") {
        return Some(environment);
    }

    if let Some(environment) = read_non_empty_env("LOGFIRE_ENVIRONMENT") {
        return Some(environment);
    }

    let resource_attrs = read_non_empty_env("OTEL_RESOURCE_ATTRIBUTES")?;
    for raw_pair in resource_attrs.split(',') {
        let Some((key, value)) = raw_pair.split_once('=') else {
            continue;
        };
        if key.trim() == "deployment.environment.name" {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub(super) fn resolve_service_version() -> Option<String> {
    read_non_empty_env("DD_VERSION")
}

pub(super) fn resolve_otel_config() -> Option<ResolvedOtelConfig> {
    let otlp_endpoint = read_non_empty_env("OTLP_ENDPOINT");
    let otel_exporter_endpoint = read_non_empty_env("OTEL_EXPORTER_OTLP_ENDPOINT");
    let logfire_token = read_non_empty_env("LOGFIRE_TOKEN");

    if std::env::var_os("OTLP_ENDPOINT").is_some() && otlp_endpoint.is_none() {
        eprintln!("OTLP_ENDPOINT is set but empty; ignoring it.");
    }
    if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some() && otel_exporter_endpoint.is_none()
    {
        eprintln!("OTEL_EXPORTER_OTLP_ENDPOINT is set but empty; ignoring it.");
    }
    if std::env::var_os("LOGFIRE_TOKEN").is_some() && logfire_token.is_none() {
        eprintln!("LOGFIRE_TOKEN is set but empty; skipping Authorization header.");
    }

    if let (Some(otlp), Some(otel_exporter)) = (&otlp_endpoint, &otel_exporter_endpoint)
        && otlp != otel_exporter
    {
        eprintln!(
            "Both OTLP_ENDPOINT and OTEL_EXPORTER_OTLP_ENDPOINT are set. Using OTLP_ENDPOINT."
        );
    }

    let (endpoint, endpoint_source) = if let Some(endpoint) = otlp_endpoint {
        (endpoint, EndpointSource::OtlpEndpoint)
    } else if let Some(endpoint) = otel_exporter_endpoint {
        (endpoint, EndpointSource::OtlpExporterEndpoint)
    } else if logfire_token.is_some() {
        (LOGFIRE_ENDPOINT.to_string(), EndpointSource::LogfireToken)
    } else {
        return None;
    };

    Some(ResolvedOtelConfig {
        endpoint,
        endpoint_source,
        logfire_token,
    })
}
