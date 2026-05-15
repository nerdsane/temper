use std::sync::Mutex;

use super::*;

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
fn continuous_tuning_clamps() {
    let _g = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("TEMPER_PROFILING_MAX_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_FREQUENCY");
    }
    assert_eq!(continuous_interval_seconds(), 300);
    assert_eq!(continuous_window_seconds(), 30);
    assert_eq!(continuous_frequency(), 100);

    unsafe {
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS", "5");
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_SECONDS", "1");
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_FREQUENCY", "1");
    }
    assert_eq!(continuous_interval_seconds(), 60);
    assert_eq!(continuous_window_seconds(), 5);
    assert_eq!(continuous_frequency(), 10);

    unsafe {
        std::env::set_var("TEMPER_PROFILING_MAX_SECONDS", "60");
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS", "999999");
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_SECONDS", "999");
        std::env::set_var("TEMPER_PROFILING_CONTINUOUS_FREQUENCY", "999");
    }
    assert_eq!(continuous_interval_seconds(), 3_600);
    assert_eq!(continuous_window_seconds(), 60);
    assert_eq!(continuous_frequency(), 500);

    unsafe {
        std::env::remove_var("TEMPER_PROFILING_MAX_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_INTERVAL_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_SECONDS");
        std::env::remove_var("TEMPER_PROFILING_CONTINUOUS_FREQUENCY");
    }
}
