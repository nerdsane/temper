//! The Observe UI: a Next.js dev server spawned alongside `temper serve`.

use std::path::Path;

use tokio::io::AsyncBufReadExt;

/// Spawn the Observe UI (Next.js dev server) in the background.
///
/// Looks for the `ui/observe/` directory relative to the binary or cwd.
/// Falls back gracefully if npm/node_modules are unavailable.
pub(crate) fn spawn(api_port: u16) {
    // The workspace root is embedded at compile time so `temper serve` works
    // regardless of the current working directory.
    const WORKSPACE_ROOT: &str = env!("CARGO_MANIFEST_DIR");

    // Try to find the observe directory from multiple locations.
    let observe_dir = None
        .or_else(|| {
            // Compile-time path: workspace_root/../../ui/observe (CARGO_MANIFEST_DIR
            // points to crates/temper-cli, so go up twice).
            let d = Path::new(WORKSPACE_ROOT)
                .parent()?
                .parent()?
                .join("ui/observe");
            if d.exists() { Some(d) } else { None }
        })
        .or_else(|| {
            // Running from the project root.
            let d = std::env::current_dir().ok()?.join("ui/observe");
            if d.exists() { Some(d) } else { None }
        })
        .or_else(|| {
            // Walk up from cwd to find a repo root containing ui/observe.
            let mut dir = std::env::current_dir().ok()?;
            loop {
                let candidate = dir.join("ui/observe");
                if candidate.exists() {
                    return Some(candidate);
                }
                if dir.join(".git").exists() {
                    return None;
                }
                if !dir.pop() {
                    return None;
                }
            }
        });

    let Some(observe_dir) = observe_dir else {
        eprintln!("  Warning: ui/observe/ directory not found, skipping Observe UI");
        return;
    };

    if !observe_dir.join("node_modules").exists() {
        eprintln!(
            "  Warning: ui/observe/node_modules not found. Run `npm install` in {} first.",
            observe_dir.display()
        );
        return;
    }

    // Find an available port starting from api_port + 1.
    // Next.js respects the PORT env var.
    let observe_port = {
        let mut port = api_port.saturating_add(1);
        loop {
            match std::net::TcpListener::bind(("0.0.0.0", port)) {
                Ok(_listener) => break port, // port is free; listener drops and releases it
                Err(_) => {
                    port = port.saturating_add(1);
                    if port > api_port.saturating_add(20) {
                        eprintln!(
                            "  Warning: no free port found for Observe UI (tried {}-{}), skipping",
                            api_port.saturating_add(1),
                            port
                        );
                        return;
                    }
                }
            }
        }
    };
    println!("  Observe UI: http://localhost:{observe_port}");
    println!();

    tokio::spawn(async move {
        let result = tokio::process::Command::new("npm")
            .arg("run")
            .arg("dev")
            .env("TEMPER_API_URL", format!("http://127.0.0.1:{api_port}"))
            .env("NEXT_PUBLIC_TEMPER_API_PORT", api_port.to_string())
            .env("PORT", observe_port.to_string())
            .current_dir(&observe_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();

        match result {
            Ok(mut child) => {
                use std::sync::Arc;
                use std::sync::atomic::{AtomicBool, Ordering};

                let opened = Arc::new(AtomicBool::new(false));

                // Drain stdout — watch for the Next.js "Ready" signal.
                if let Some(stdout) = child.stdout.take() {
                    let opened = opened.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(stdout).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if !opened.load(Ordering::Relaxed)
                                && (line.contains("Ready in") || line.contains("started server on"))
                            {
                                opened.store(true, Ordering::Relaxed);
                                let _ = open::that(format!("http://localhost:{observe_port}"));
                            }
                        }
                    });
                }

                // Drain stderr — report errors.
                if let Some(stderr) = child.stderr.take() {
                    let opened = opened.clone();
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            // Next.js may also signal readiness on stderr.
                            if !opened.load(Ordering::Relaxed)
                                && (line.contains("Ready in") || line.contains("started server on"))
                            {
                                opened.store(true, Ordering::Relaxed);
                                let _ = open::that(format!("http://localhost:{observe_port}"));
                            }
                            if line.contains("error") || line.contains("Error") {
                                eprintln!("  [observe] {line}");
                            }
                        }
                    });
                }

                let _ = child.wait().await;
            }
            Err(e) => {
                eprintln!("  Warning: failed to start Observe UI: {e}");
            }
        }
    });
}
