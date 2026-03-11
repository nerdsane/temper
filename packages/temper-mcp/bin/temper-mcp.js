#!/usr/bin/env node

const { execFileSync } = require("child_process");
const os = require("os");
const path = require("path");

const PLATFORM_PACKAGES = {
  "darwin-arm64": "temper-mcp-darwin-arm64",
  "darwin-x64": "temper-mcp-darwin-x64",
  "linux-x64": "temper-mcp-linux-x64",
  "linux-arm64": "temper-mcp-linux-arm64",
  "win32-x64": "temper-mcp-win32-x64",
};

const platformKey = `${os.platform()}-${os.arch()}`;
const pkg = PLATFORM_PACKAGES[platformKey];

if (!pkg) {
  console.error(
    `Unsupported platform: ${platformKey}. ` +
    `Supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}`
  );
  process.exit(1);
}

let binPath;
try {
  const binName = os.platform() === "win32" ? "temper.exe" : "temper";
  binPath = path.join(
    path.dirname(require.resolve(`${pkg}/package.json`)),
    "bin",
    binName
  );
} catch {
  console.error(
    `Could not find the binary package ${pkg}. ` +
    `This usually means the optional dependency was not installed.\n` +
    `Try reinstalling: npm install temper-mcp`
  );
  process.exit(1);
}

// The binary is the full temper CLI; invoke its "mcp" subcommand
const args = ["mcp", ...process.argv.slice(2)];

try {
  execFileSync(binPath, args, { stdio: "inherit" });
} catch (e) {
  if (e.status !== null) {
    process.exit(e.status);
  }
  throw e;
}
