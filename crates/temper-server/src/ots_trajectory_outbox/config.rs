use std::time::Duration;

use super::OtsTrajectoryOutboxConfig;

const DEFAULT_CAPACITY: usize = 512;
const DEFAULT_DRAIN_BATCH: usize = 16;
const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_RETRY_DELAY_MS: u64 = 100;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name) // determinism-ok: observe persistence queue config read at startup
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub(super) fn outbox_config() -> OtsTrajectoryOutboxConfig {
    OtsTrajectoryOutboxConfig {
        capacity: env_usize("TEMPER_OTS_TRAJECTORY_OUTBOX_CAPACITY", DEFAULT_CAPACITY),
        drain_batch: env_usize("TEMPER_OTS_TRAJECTORY_DRAIN_BATCH", DEFAULT_DRAIN_BATCH),
        max_attempts: env_u32("TEMPER_OTS_TRAJECTORY_MAX_ATTEMPTS", DEFAULT_MAX_ATTEMPTS),
        retry_delay: Duration::from_millis(env_u64(
            "TEMPER_OTS_TRAJECTORY_RETRY_DELAY_MS",
            DEFAULT_RETRY_DELAY_MS,
        )),
    }
}
