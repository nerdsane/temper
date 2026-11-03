//! The host's memory, its disks and the size of chosen directory trees go out
//! with the runtime metrics. Every test reads them back from the export a
//! collector receives, after one sampling pass with the given settings.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Gauge;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

use super::RuntimeMetricInstruments;
use crate::state::ServerState;

const DISK_PATHS: &str = "TEMPER_RUNTIME_METRICS_DISK_PATHS";
const DIR_PATHS: &str = "TEMPER_RUNTIME_METRICS_DIR_PATHS";
const DIR_TIMEOUT_MS: &str = "TEMPER_RUNTIME_METRICS_DIR_TIMEOUT_MS";

const DISK_USED_RATIO: &str = "temper_host_disk_used_ratio";
const DIR_BYTES: &str = "temper_host_dir_bytes";

/// How long a test waits for a directory walk, which runs beside the other
/// readings and reports when it finishes.
const WALK_WAIT_POLLS: usize = 100;
const WALK_WAIT_POLL_MS: u64 = 50;

/// The settings are process environment and the meter provider is
/// process-global, so the tests here take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// What one sampling pass exported.
struct Export {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

impl Export {
    /// Run one sampling pass with these settings; the other host settings are unset.
    async fn sample(settings: &[(&str, &str)]) -> Self {
        for name in [DISK_PATHS, DIR_PATHS, DIR_TIMEOUT_MS] {
            // SAFETY: every test that touches these variables holds TURN, and
            // nothing else in this binary reads them.
            unsafe { std::env::remove_var(name) };
        }
        for (name, value) in settings {
            // SAFETY: as above.
            unsafe { std::env::set_var(name, value) };
        }
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        let instruments = RuntimeMetricInstruments::new();
        instruments.record(&test_state()).await;
        Self { provider, exporter }
    }

    /// The last value of each series of a gauge, keyed by its `path`
    /// attribute (empty when it has none).
    fn gauge<T>(&self, name: &str) -> BTreeMap<String, T>
    where
        T: Copy + std::fmt::Debug + Send + Sync + 'static,
    {
        self.provider.force_flush().expect("flush metrics");
        let mut series = BTreeMap::new();
        for resource in self.exporter.get_finished_metrics().expect("metrics") {
            for scope in &resource.scope_metrics {
                for metric in scope.metrics.iter().filter(|m| m.name == name) {
                    let gauge = metric
                        .data
                        .as_any()
                        .downcast_ref::<Gauge<T>>()
                        .unwrap_or_else(|| panic!("{name} is a gauge of this value type"));
                    for point in &gauge.data_points {
                        series.insert(path_of(&point.attributes), point.value);
                    }
                }
            }
        }
        series
    }

    /// The value a directory walk reports for `path`, waiting for the walk.
    async fn walked(&self, path: &str) -> Option<u64> {
        for _ in 0..WALK_WAIT_POLLS {
            if let Some(bytes) = self.gauge::<u64>(DIR_BYTES).get(path) {
                return Some(*bytes);
            }
            tokio::time::sleep(Duration::from_millis(WALK_WAIT_POLL_MS)).await;
        }
        None
    }
}

fn test_state() -> ServerState {
    let csdl_xml = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).expect("the fixture CSDL parses");
    ServerState::new(
        ActorSystem::new("host-metrics-test"),
        csdl,
        csdl_xml.to_string(),
    )
}

fn path_of(attributes: &[KeyValue]) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == "path")
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

/// Used space as `df` counts it: what is used, of what is used plus what an
/// ordinary process may still write.
#[cfg(unix)]
fn df_used_ratio(path: &Path) -> f64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
    // SAFETY: statvfs only writes into the zeroed struct it is given.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and stat is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    assert_eq!(rc, 0, "statvfs({})", path.display());
    let used = stat.f_blocks as f64 - stat.f_bfree as f64;
    used / (used + stat.f_bavail as f64)
}

/// Bytes these entries take on disk, as `du` counts them.
#[cfg(unix)]
fn disk_bytes(entries: &[PathBuf]) -> u64 {
    use std::os::unix::fs::MetadataExt;

    entries
        .iter()
        .map(|entry| {
            std::fs::symlink_metadata(entry)
                .expect("the entry exists")
                .blocks()
                * 512
        })
        .sum()
}

fn write_bytes(path: &Path, len: usize) {
    std::fs::write(path, vec![7u8; len]).expect("write a test file");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_whole_machine_memory_is_reported_from_proc_meminfo() {
    let _turn = TURN.lock().await;
    let export = Export::sample(&[]).await;

    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("procfs");
    let kb = |field: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(field))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("{field} in /proc/meminfo"))
    };
    let (total_kb, available_kb) = (kb("MemTotal"), kb("MemAvailable"));

    assert_eq!(
        export
            .gauge::<u64>("temper_host_memory_total_bytes")
            .get(""),
        Some(&(total_kb * 1024)),
        "the machine's memory in bytes, from MemTotal"
    );
    let used = export
        .gauge::<f64>("temper_host_memory_used_ratio")
        .get("")
        .copied()
        .expect("the share of the machine's memory in use");
    let expected = 1.0 - available_kb as f64 / total_kb as f64;
    assert!(
        used > 0.0 && used < 1.0 && (used - expected).abs() < 0.05,
        "used ratio {used} is (MemTotal - MemAvailable) / MemTotal, about {expected}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_root_disk_is_reported_by_default_and_no_tree_is_walked() {
    let _turn = TURN.lock().await;
    let export = Export::sample(&[]).await;

    let disks = export.gauge::<f64>(DISK_USED_RATIO);
    assert_eq!(
        disks.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["/"],
        "with no setting, only the root disk"
    );
    let ratio = disks["/"];
    let expected = df_used_ratio(Path::new("/"));
    assert!(
        (0.0..=1.0).contains(&ratio) && (ratio - expected).abs() < 0.01,
        "root disk used ratio {ratio}, as df counts it about {expected}"
    );

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        export.gauge::<u64>(DIR_BYTES).is_empty(),
        "no directory is walked unless one is configured"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn each_configured_disk_is_reported_and_one_that_cannot_be_read_is_left_out() {
    let _turn = TURN.lock().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let readable = dir.path().display().to_string();
    let missing = dir.path().join("missing").display().to_string();
    let setting = format!("{readable},{missing}");
    let export = Export::sample(&[(DISK_PATHS, &setting)]).await;

    let disks = export.gauge::<f64>(DISK_USED_RATIO);
    assert_eq!(
        disks.keys().cloned().collect::<Vec<_>>(),
        vec![readable.clone()],
        "the setting replaces the default, and a path that cannot be read is left out"
    );
    let ratio = disks[&readable];
    let expected = df_used_ratio(dir.path());
    assert!(
        (ratio - expected).abs() < 0.01,
        "used ratio {ratio} of {readable}, as df counts it about {expected}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dir_bytes_add_up_every_tree_a_star_segment_matches() {
    let _turn = TURN.lock().await;
    let root = tempfile::tempdir().expect("tempdir");
    let mounts = root.path().join("mounts");
    let first = mounts.join("m-1").join("logical-v1").join("data");
    let second = mounts.join("m-2").join("logical-v1").join("data");
    std::fs::create_dir_all(first.join("nested")).expect("mkdir");
    std::fs::create_dir_all(&second).expect("mkdir");
    // A mount with no data tree, and a file where a directory is expected.
    std::fs::create_dir_all(mounts.join("m-3")).expect("mkdir");
    write_bytes(&mounts.join("stray"), 10);
    write_bytes(&first.join("one"), 5_000);
    write_bytes(&first.join("nested").join("two"), 100);
    write_bytes(&second.join("three"), 70_000);
    // Outside the pattern, reached only through a symbolic link that is not followed.
    let outside = mounts.join("m-1").join("logical-v1").join("other");
    write_bytes(&outside, 1_000_000);
    std::os::unix::fs::symlink(&outside, first.join("link")).expect("symlink");

    let pattern = format!("{}/*/logical-v1/data", mounts.display());
    let no_match = format!("{}/nothing/*/data", root.path().display());
    let setting = format!("{no_match},{pattern}");
    let export = Export::sample(&[(DIR_PATHS, &setting)]).await;

    let expected = disk_bytes(&[
        first.clone(),
        first.join("nested"),
        first.join("one"),
        first.join("nested").join("two"),
        first.join("link"),
        second.clone(),
        second.join("three"),
    ]);
    assert!(expected >= 70_000, "the test files are on disk: {expected}");
    assert_eq!(
        export.walked(&pattern).await,
        Some(expected),
        "one series for the pattern: every matching tree added up, links not followed"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        export
            .gauge::<u64>(DIR_BYTES)
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![pattern],
        "a pattern that matches nothing reports nothing"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_walk_out_of_time_reports_nothing_and_the_other_readings_still_go_out() {
    let _turn = TURN.lock().await;
    let tree = tempfile::tempdir().expect("tempdir");
    write_bytes(&tree.path().join("file"), 4_096);
    let path = tree.path().display().to_string();
    let export = Export::sample(&[(DIR_PATHS, &path), (DIR_TIMEOUT_MS, "0")]).await;

    assert!(
        export.gauge::<f64>(DISK_USED_RATIO).contains_key("/"),
        "the disk reading goes out without waiting for the walk"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        export.gauge::<u64>(DIR_BYTES).is_empty(),
        "a walk stopped by its time limit publishes no partial size"
    );
}
