#!/usr/bin/env python3
"""Local sandbox server for Temper Agent development.

Implements the sandbox HTTP API that the tool_runner WASM module targets:
  GET  /v1/fs/file?path=...   → read file
  PUT  /v1/fs/file?path=...   → write file
  POST /v1/processes/run       → execute bash command
  GET  /health                 → health check

No isolation — runs directly on the host filesystem and shell.
For production, use E2B sandboxes via the sandbox_provisioner WASM module.

Usage:
  python3 local_sandbox.py [--port 9999] [--workdir /tmp/sandbox]
"""

import argparse
import json
import os
import subprocess
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from urllib.parse import urlparse, parse_qs


class SandboxHandler(BaseHTTPRequestHandler):
    """HTTP handler implementing the sandbox API."""

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._json(200, {"status": "ok"})
            return
        if parsed.path == "/v1/fs/file":
            path = parse_qs(parsed.query).get("path", [None])[0]
            if not path:
                self._json(400, {"error": "missing path parameter"})
                return
            full = self._resolve(path)
            if not os.path.isfile(full):
                self._json(404, {"error": f"not found: {path}"})
                return
            try:
                with open(full, "r") as f:
                    content = f.read()
            except Exception as e:
                self._json(500, {"error": str(e)})
                return
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(content.encode())
            return
        self._json(404, {"error": "unknown endpoint"})

    def do_PUT(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/fs/file":
            path = parse_qs(parsed.query).get("path", [None])[0]
            if not path:
                self._json(400, {"error": "missing path parameter"})
                return
            full = self._resolve(path)
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode() if length else ""
            try:
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w") as f:
                    f.write(body)
            except Exception as e:
                self._json(500, {"error": str(e)})
                return
            self._json(200, {"status": "ok", "path": path})
            return
        self._json(404, {"error": "unknown endpoint"})

    def do_POST(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/processes/run":
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode() if length else "{}"
            try:
                req = json.loads(body)
            except json.JSONDecodeError as e:
                self._json(400, {"error": f"invalid JSON: {e}"})
                return
            cmd = req.get("command", "")
            cwd = req.get("workdir", self.server.workdir)
            if not cmd:
                self._json(400, {"error": "missing command"})
                return
            try:
                r = subprocess.run(
                    cmd, shell=True, capture_output=True, text=True,
                    timeout=60, cwd=cwd
                )
                self._json(200, {
                    "stdout": r.stdout,
                    "stderr": r.stderr,
                    "exit_code": r.returncode,
                })
            except subprocess.TimeoutExpired:
                self._json(200, {
                    "stdout": "",
                    "stderr": "command timed out after 60s",
                    "exit_code": -1,
                })
            except Exception as e:
                self._json(500, {"error": str(e)})
            return
        self._json(404, {"error": "unknown endpoint"})

    def _resolve(self, path):
        if path.startswith("/"):
            return path
        return os.path.join(self.server.workdir, path)

    def _json(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        print(f"[sandbox] {fmt % args}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(
        description="Local sandbox server for Temper Agent"
    )
    parser.add_argument(
        "--port", type=int, default=9999,
        help="Port to listen on (default: 9999)"
    )
    parser.add_argument(
        "--workdir", type=str, default="/tmp/sandbox",
        help="Working directory for file operations (default: /tmp/sandbox)"
    )
    args = parser.parse_args()

    os.makedirs(args.workdir, exist_ok=True)

    server = HTTPServer(("0.0.0.0", args.port), SandboxHandler)
    server.workdir = args.workdir

    print(f"Local sandbox server listening on :{args.port}", file=sys.stderr)
    print(f"Working directory: {args.workdir}", file=sys.stderr)
    print(f"Endpoints:", file=sys.stderr)
    print(f"  GET  http://localhost:{args.port}/health", file=sys.stderr)
    print(f"  GET  http://localhost:{args.port}/v1/fs/file?path=...", file=sys.stderr)
    print(f"  PUT  http://localhost:{args.port}/v1/fs/file?path=...", file=sys.stderr)
    print(f"  POST http://localhost:{args.port}/v1/processes/run", file=sys.stderr)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.", file=sys.stderr)
        server.server_close()


if __name__ == "__main__":
    main()
