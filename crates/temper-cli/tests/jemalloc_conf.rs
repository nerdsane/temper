//! The binary's jemalloc options take effect and leave the allocator fully
//! started: no jemalloc warning, more than one arena, the decay settings in
//! force, and background purging threads where they are supported (Linux).

use std::process::Command;

#[test]
fn jemalloc_starts_with_the_purge_options() {
    let output = Command::new(env!("CARGO_BIN_EXE_temper"))
        .arg("--help")
        .env("_RJEM_MALLOC_CONF", "stats_print:true,stats_print_opts:J")
        .output()
        .expect("temper runs");
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.lines().any(|line| line.starts_with("<jemalloc>:")),
        "jemalloc warned at startup:\n{stderr}"
    );

    let start = stderr
        .find('{')
        .expect("jemalloc printed its stats as JSON");
    let stats: serde_json::Value =
        serde_json::from_str(stderr[start..].trim()).expect("stats are JSON");
    let jemalloc = &stats["jemalloc"];
    let opt = &jemalloc["opt"];
    assert_eq!(opt["dirty_decay_ms"], 1000, "options: {opt}");
    assert_eq!(opt["muzzy_decay_ms"], 0, "options: {opt}");
    assert!(
        jemalloc["arenas"]["narenas"].as_u64().unwrap_or(0) > 1,
        "a single arena means jemalloc did not finish starting"
    );
    // Background purging threads exist only where jemalloc supports them;
    // elsewhere the option is not even reported.
    if cfg!(target_os = "linux") {
        assert_eq!(opt["background_thread"], true, "options: {opt}");
    } else {
        assert_ne!(opt["background_thread"], true, "options: {opt}");
    }
}
