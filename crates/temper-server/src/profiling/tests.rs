use std::sync::Mutex;

use pprof::protos::{Message, Profile, ValueType};

use super::capture::{encode, name_sample_types};
use super::config;
use super::upload::{Captured, PROFILE_ATTACHMENT, event_json, upload};

// Cargo runs tests in parallel; environment changes race otherwise.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const SETTINGS: [&str; 17] = [
    "TEMPER_PROFILING_ENABLED",
    "TEMPER_PROFILING_CONTINUOUS",
    "TEMPER_PROFILING_MAX_SECONDS",
    "TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS",
    "TEMPER_PROFILING_CONTINUOUS_SECONDS",
    "TEMPER_PROFILING_CONTINUOUS_FREQUENCY",
    "TEMPER_PROFILING_INTAKE_URL",
    "TEMPER_PROFILING_INTAKE_HEADERS",
    "DD_AGENT_HOST",
    "DD_TRACE_AGENT_PORT",
    "DD_TRACE_AGENT_URL",
    "DD_SERVICE",
    "DD_ENV",
    "DD_VERSION",
    "DD_TAGS",
    "OTEL_SERVICE_NAME",
    "BUILD_VERSION",
];

/// Run `body` with exactly `vars` set among the profiler's settings.
fn with_env(vars: &[(&str, &str)], body: impl FnOnce()) {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let set = |vars: &[(&str, &str)]| unsafe {
        for name in SETTINGS {
            std::env::remove_var(name);
        }
        for (name, value) in vars {
            std::env::set_var(name, value);
        }
    };
    set(vars);
    body();
    set(&[]);
}

#[test]
fn switches_are_off_until_set() {
    with_env(&[], || {
        assert!(!config::on_demand_enabled());
        assert!(!config::continuous_enabled());
    });
    for truthy in ["1", "true", "yes", "on"] {
        with_env(&[("TEMPER_PROFILING_CONTINUOUS", truthy)], || {
            assert!(config::continuous_enabled(), "{truthy} turns it on");
        });
    }
    for falsy in ["0", "false", "no", "off", ""] {
        with_env(&[("TEMPER_PROFILING_CONTINUOUS", falsy)], || {
            assert!(!config::continuous_enabled(), "{falsy:?} leaves it off");
        });
    }
}

#[test]
fn windows_and_rates_stay_in_bounds() {
    with_env(&[], || {
        assert_eq!(config::max_window_seconds(), 120);
        assert_eq!(config::continuous_interval_seconds(), 300);
        assert_eq!(config::continuous_window_seconds(), 30);
        assert_eq!(config::continuous_frequency(), 100);
    });
    with_env(
        &[
            ("TEMPER_PROFILING_MAX_SECONDS", "2"),
            ("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS", "5"),
            ("TEMPER_PROFILING_CONTINUOUS_SECONDS", "1"),
            ("TEMPER_PROFILING_CONTINUOUS_FREQUENCY", "1"),
        ],
        || {
            assert_eq!(config::max_window_seconds(), 5);
            assert_eq!(config::continuous_interval_seconds(), 60);
            assert_eq!(config::continuous_window_seconds(), 5);
            assert_eq!(config::continuous_frequency(), 10);
        },
    );
    with_env(
        &[
            ("TEMPER_PROFILING_MAX_SECONDS", "60"),
            ("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS", "999999"),
            ("TEMPER_PROFILING_CONTINUOUS_SECONDS", "999"),
            ("TEMPER_PROFILING_CONTINUOUS_FREQUENCY", "99999999999"),
        ],
        || {
            assert_eq!(config::continuous_interval_seconds(), 3_600);
            assert_eq!(config::continuous_window_seconds(), 60);
            assert_eq!(config::continuous_frequency(), 500);
        },
    );
}

#[test]
fn intake_is_found_as_datadogs_libraries_find_their_agent() {
    // 4. Nothing set: no intake, and no address of the kernel's own.
    with_env(&[], || assert_eq!(config::intake(), None));
    // A port alone names no agent.
    with_env(&[("DD_TRACE_AGENT_PORT", "9126")], || {
        assert_eq!(config::intake(), None)
    });
    // 3. DD_AGENT_HOST, with DD_TRACE_AGENT_PORT or the default port.
    with_env(&[("DD_AGENT_HOST", "agent.internal")], || {
        let intake = config::intake().expect("an agent host is set");
        assert_eq!(intake.url, "http://agent.internal:8126/profiling/v1/input");
        assert!(intake.headers.is_empty());
    });
    with_env(
        &[
            ("DD_AGENT_HOST", "agent.internal"),
            ("DD_TRACE_AGENT_PORT", "9126"),
        ],
        || {
            assert_eq!(
                config::intake().map(|intake| intake.url),
                Some("http://agent.internal:9126/profiling/v1/input".to_string())
            )
        },
    );
    // 2. DD_TRACE_AGENT_URL wins over the host and port.
    with_env(
        &[
            ("DD_TRACE_AGENT_URL", "http://agent.example:8126/"),
            ("DD_AGENT_HOST", "agent.internal"),
        ],
        || {
            assert_eq!(
                config::intake().map(|intake| intake.url),
                Some("http://agent.example:8126/profiling/v1/input".to_string())
            )
        },
    );
    // 1. TEMPER_PROFILING_INTAKE_URL wins over every agent setting, with its headers.
    with_env(
        &[
            ("DD_TRACE_AGENT_URL", "http://agent.example:8126"),
            ("DD_AGENT_HOST", "agent.internal"),
            (
                "TEMPER_PROFILING_INTAKE_URL",
                "https://collector.example/profiling/v1/input",
            ),
            (
                "TEMPER_PROFILING_INTAKE_HEADERS",
                "authorization=Bearer abc, x-extra = 1",
            ),
        ],
        || {
            let intake = config::intake().expect("an intake is configured");
            assert_eq!(intake.url, "https://collector.example/profiling/v1/input");
            assert_eq!(
                intake.headers,
                vec![
                    ("authorization".to_string(), "Bearer abc".to_string()),
                    ("x-extra".to_string(), "1".to_string()),
                ]
            );
        },
    );
}

fn captured(profile_type: &'static str) -> Captured {
    let at = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .unwrap()
            .with_timezone(&chrono::Utc)
    };
    Captured {
        pprof: vec![1, 2, 3],
        profile_type,
        mode: "continuous",
        started_at: at("2026-11-07T19:39:42Z"),
        ended_at: at("2026-11-07T19:40:12Z"),
    }
}

#[test]
fn event_follows_datadogs_native_profile_envelope() {
    with_env(
        &[
            ("DD_SERVICE", "app-temper"),
            ("DD_ENV", "staging"),
            ("DD_VERSION", "30feec4"),
            ("DD_TAGS", "app_id:a1,temper_id:t1"),
            ("TEMPER_PROFILING_CONTINUOUS", "true"),
        ],
        || {
            let event = event_json(&captured("heap"));
            assert_eq!(event["version"], "4");
            assert_eq!(event["family"], "native");
            assert_eq!(event["start"], "2026-11-07T19:39:42Z");
            assert_eq!(event["end"], "2026-11-07T19:40:12Z");
            assert_eq!(
                event["attachments"],
                serde_json::json!([PROFILE_ATTACHMENT])
            );
            assert_eq!(event["info"]["profiler"]["activation"], "auto");
            let tags = event["tags_profiler"].as_str().unwrap();
            for expected in [
                "service:app-temper",
                "env:staging",
                "version:30feec4",
                "runtime:rust",
                "profile.component:heap",
                "profile.mode:continuous",
                "app_id:a1",
                "temper_id:t1",
                "runtime-id:",
            ] {
                assert!(tags.contains(expected), "missing {expected} in {tags}");
            }
        },
    );
}

#[test]
fn tags_name_no_environment_or_version_nobody_set() {
    with_env(&[("OTEL_SERVICE_NAME", "my-app")], || {
        let tags = super::upload::profile_tags("cpu", "continuous");
        assert_eq!(tags[0], "service:my-app");
        assert!(
            !tags
                .iter()
                .any(|t| t.starts_with("env:") || t.starts_with("version:")),
            "{tags:?}"
        );
    });
    with_env(
        &[
            ("OTEL_SERVICE_NAME", "my-app"),
            ("DD_SERVICE", "app-temper"),
        ],
        || {
            assert_eq!(
                super::upload::profile_tags("cpu", "continuous")[0],
                "service:app-temper"
            )
        },
    );
}

#[tokio::test(flavor = "current_thread")]
async fn upload_reaches_the_intake_with_its_headers() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let intake = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/profiling/v1/input"))
        .and(header("authorization", "Bearer abc"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&intake)
        .await;
    let to = config::Intake {
        url: format!("{}/profiling/v1/input", intake.uri()),
        headers: vec![("authorization".to_string(), "Bearer abc".to_string())],
    };
    upload(captured("cpu"), to).await;

    let requests = intake.received_requests().await.expect("recording on");
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(
        body.contains("name=\"event\"; filename=\"event.json\""),
        "{body}"
    );
    assert!(
        body.contains("name=\"profile.pprof\"; filename=\"profile.pprof\""),
        "{body}"
    );
}

#[test]
fn sample_types_take_datadog_names_and_still_decode() {
    let mut profile = Profile {
        string_table: vec![
            String::new(),
            "samples".to_string(),
            "count".to_string(),
            "cpu".to_string(),
            "nanoseconds".to_string(),
        ],
        sample_type: vec![ValueType { ty: 1, unit: 2 }, ValueType { ty: 3, unit: 4 }],
        period: 10_000_000,
        ..Default::default()
    };
    name_sample_types(
        &mut profile,
        &[("cpu-samples", "count"), ("cpu-time", "nanoseconds")],
    );
    let decoded = Profile::decode(encode(&profile).unwrap().as_slice()).unwrap();
    let name = |i: i64| decoded.string_table[i as usize].as_str();
    let types: Vec<(&str, &str)> = decoded
        .sample_type
        .iter()
        .map(|t| (name(t.ty), name(t.unit)))
        .collect();
    assert_eq!(
        types,
        [("cpu-samples", "count"), ("cpu-time", "nanoseconds")]
    );
    assert_eq!(name(3), "cpu", "existing strings keep their meaning");
}

/// A profile of one function per frame, leaf first, one sample per stack.
fn profile_of(stacks: &[&[&str]]) -> Profile {
    use pprof::protos::{Function, Line, Location, Sample};
    let mut profile = Profile {
        string_table: vec![String::new()],
        ..Default::default()
    };
    for stack in stacks {
        let mut ids = Vec::new();
        for name in *stack {
            let id = profile.function.len() as u64 + 1;
            profile.string_table.push((*name).to_string());
            profile.function.push(Function {
                id,
                name: profile.string_table.len() as i64 - 1,
                ..Default::default()
            });
            profile.location.push(Location {
                id,
                line: vec![Line {
                    function_id: id,
                    ..Default::default()
                }],
                ..Default::default()
            });
            ids.push(id);
        }
        profile.sample.push(Sample {
            location_id: ids,
            value: vec![1 << 19],
            label: Vec::new(),
        });
    }
    profile
}

#[test]
fn heap_stacks_start_at_the_code_that_allocated() {
    let mut profile = profile_of(&[
        &[
            "prof_backtrace_impl",
            "_rjem_je_prof_backtrace",
            "_rjem_je_prof_tctx_create",
            "do_rallocx",
            "alloc::raw_vec::finish_grow",
            "app::load",
        ],
        &["already::caller", "main"],
    ]);
    super::heap::start_at_the_caller(&mut profile);
    let leaf_names: Vec<Vec<&str>> = profile
        .sample
        .iter()
        .map(|sample| {
            sample
                .location_id
                .iter()
                .map(|id| {
                    let function = &profile.function[*id as usize - 1];
                    profile.string_table[function.name as usize].as_str()
                })
                .collect()
        })
        .collect();
    assert_eq!(
        leaf_names,
        [
            vec!["alloc::raw_vec::finish_grow", "app::load"],
            vec!["already::caller", "main"],
        ]
    );
}
