import { spawn, ChildProcess } from "node:child_process";

let serverProcess: ChildProcess | null = null;
let serverPort: number | null = null;

/**
 * Start a Temper server on a random port for testing.
 * Returns the base URL (e.g., "http://localhost:4201").
 */
export async function startServer(): Promise<string> {
  if (serverProcess) {
    return `http://localhost:${serverPort}`;
  }

  return new Promise((resolve, reject) => {
    const proc = spawn("cargo", ["run", "-p", "temper-cli", "--", "serve", "--port", "0", "--storage", "turso"], {
      cwd: process.env.TEMPER_ROOT ?? findWorkspaceRoot(),
      stdio: ["ignore", "pipe", "inherit"],
    });

    let resolved = false;
    const timeout = setTimeout(() => {
      if (!resolved) {
        reject(new Error("Server startup timed out after 60s"));
        proc.kill();
      }
    }, 60_000);

    proc.stdout?.on("data", (data: Buffer) => {
      const line = data.toString();
      const match = line.match(/Listening on http:\/\/0\.0\.0\.0:(\d+)/);
      if (match && !resolved) {
        resolved = true;
        clearTimeout(timeout);
        serverPort = parseInt(match[1], 10);
        serverProcess = proc;
        resolve(`http://localhost:${serverPort}`);
      }
    });

    proc.on("error", (err) => {
      if (!resolved) {
        resolved = true;
        clearTimeout(timeout);
        reject(err);
      }
    });

    proc.on("exit", (code) => {
      if (!resolved) {
        resolved = true;
        clearTimeout(timeout);
        reject(new Error(`Server exited with code ${code} before starting`));
      }
      serverProcess = null;
      serverPort = null;
    });
  });
}

/**
 * Stop the running Temper server.
 */
export async function stopServer(): Promise<void> {
  if (serverProcess) {
    serverProcess.kill("SIGTERM");
    serverProcess = null;
    serverPort = null;
    // Give it a moment to shut down cleanly
    await new Promise((r) => setTimeout(r, 500));
  }
}

/**
 * Get the URL of the running server.
 */
export function getServerUrl(): string {
  if (!serverPort) throw new Error("Server not started");
  return `http://localhost:${serverPort}`;
}

function findWorkspaceRoot(): string {
  let dir = process.cwd();
  while (dir !== "/") {
    try {
      const cargoToml = require("node:fs").readFileSync(`${dir}/Cargo.toml`, "utf-8");
      if (cargoToml.includes("[workspace]")) return dir;
    } catch { /* not found */ }
    dir = require("node:path").dirname(dir);
  }
  throw new Error("Could not find Temper workspace root");
}
