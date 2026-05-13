use super::*;
use std::sync::Mutex;

// Cargo runs tests in parallel; env-var mutation races otherwise.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn default_values_applied() {
    let q = CpuProfileQuery {
        seconds: default_seconds(),
        frequency: default_frequency(),
    };
    assert_eq!(q.seconds, 30);
    assert_eq!(q.frequency, 100);
}

#[test]
fn profiling_toggle_states() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("TEMPER_PROFILING_ENABLED");
    }
    assert!(!profiling_enabled(), "default should be off");

    for truthy in ["1", "true", "yes", "on"] {
        unsafe {
            std::env::set_var("TEMPER_PROFILING_ENABLED", truthy);
        }
        assert!(profiling_enabled(), "{truthy} should enable");
    }

    for falsy in ["0", "false", "no", "off", ""] {
        unsafe {
            std::env::set_var("TEMPER_PROFILING_ENABLED", falsy);
        }
        assert!(!profiling_enabled(), "{falsy:?} should disable");
    }

    unsafe {
        std::env::remove_var("TEMPER_PROFILING_ENABLED");
    }
}

#[test]
fn max_window_clamps() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("TEMPER_PROFILING_MAX_SECONDS", "99999");
    }
    assert_eq!(max_window_seconds(), 600);
    unsafe {
        std::env::set_var("TEMPER_PROFILING_MAX_SECONDS", "2");
    }
    assert_eq!(max_window_seconds(), 5);
    unsafe {
        std::env::remove_var("TEMPER_PROFILING_MAX_SECONDS");
    }
}

#[test]
fn datadog_profile_event_uses_agent_intake_envelope() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("DD_SERVICE", "temperpaw");
        std::env::set_var("DD_ENV", "prod");
        std::env::set_var("DD_VERSION", "30feec4");
        std::env::remove_var("DD_PROFILING_ENABLED");
    }

    let started_at = chrono::DateTime::parse_from_rfc3339("2026-05-12T19:39:42Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let ended_at = chrono::DateTime::parse_from_rfc3339("2026-05-12T19:39:47Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let filename = profile_filename("cpu");
    let event = profile_upload_event_json("cpu", &filename, started_at, ended_at);

    assert_eq!(event["version"], "4");
    assert_eq!(event["family"], "rust");
    assert_eq!(event["start"], "2026-05-12T19:39:42Z");
    assert_eq!(event["end"], "2026-05-12T19:39:47Z");
    assert_eq!(event["attachments"], serde_json::json!(["cpu.pprof"]));
    assert_eq!(event["info"]["profiler"]["activation"], "manual");
    assert_eq!(event["info"]["profiler"]["ssi"]["mechanism"], "none");
    assert_eq!(event["info"]["profiler"]["settings"]["profile_type"], "cpu");

    let tags = event["tags_profiler"].as_str().unwrap();
    for expected in [
        "service:temperpaw",
        "env:prod",
        "version:30feec4",
        "runtime:rust",
        "profile.component:cpu",
    ] {
        assert!(tags.contains(expected), "missing tag {expected} in {tags}");
    }
    assert!(
        tags.contains("runtime-id:"),
        "runtime-id tag missing in {tags}"
    );

    unsafe {
        std::env::remove_var("DD_SERVICE");
        std::env::remove_var("DD_ENV");
        std::env::remove_var("DD_VERSION");
    }
}
