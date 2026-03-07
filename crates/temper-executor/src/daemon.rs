//! Daemonization support for the executor.

use anyhow::{Context, Result};

/// Double-fork daemonization with PID file.
pub fn daemonize() -> Result<()> {
    use std::fs;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    // Create PID file directory.
    let pid_dir = dirs_pid_dir();
    fs::create_dir_all(&pid_dir).context("Failed to create PID directory")?;

    // Fork: the child continues, the parent exits.
    // We use a re-exec approach instead of raw fork for safety.
    let args: Vec<String> = std::env::args().filter(|a| a != "--detach").collect();
    let child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .context("Failed to spawn daemon process")?;

    // Write PID file.
    let pid_file = pid_dir.join("executor.pid");
    fs::write(&pid_file, child.id().to_string()).context("Failed to write PID file")?;
    eprintln!(
        "Executor daemonized. PID={}, PID file={}",
        child.id(),
        pid_file.display()
    );

    // The parent exits immediately.
    std::process::exit(0);
}

/// Clean up PID file on shutdown.
pub fn cleanup_pid_file() {
    let pid_file = dirs_pid_dir().join("executor.pid");
    if pid_file.exists() {
        std::fs::remove_file(&pid_file).ok();
    }
}

/// PID file directory: ~/.local/state/temper/
fn dirs_pid_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()); // determinism-ok: executor process
    std::path::PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("temper")
}
