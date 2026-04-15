#!/usr/bin/env python3
"""Local sandbox HTTP server for Temper Agent E2E testing.

Implements the sandbox API that the tool_runner WASM module targets:
  GET  /v1/fs/file?path=...        → read file
  PUT  /v1/fs/file?path=...        → write file
  POST /v1/processes/run            → execute bash command
  GET  /health                      → health check

Usage:
  python3 local_server.py [--port 9999] [--workdir /tmp/sandbox]
"""

import argparse
import json
import os
import subprocess
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse, parse_qs


class SandboxHandler(BaseHTTPRequestHandler):
    workdir: str = "/tmp/temper-sandbox"

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._json_response(200, {"status": "ok"})
            return
        if parsed.path == "/v1/fs/file":
            params = parse_qs(parsed.query)
            file_path = params.get("path", [None])[0]
            if not file_path:
                self._json_response(400, {"error": "missing path parameter"})
                return
            full_path = self._resolve_path(file_path)
            if not os.path.isfile(full_path):
                self._json_response(404, {"error": f"file not found: {file_path}"})
                return
            try:
                with open(full_path, "r") as f:
                    content = f.read()
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(content.encode())
            except Exception as e:
                self._json_response(500, {"error": str(e)})
            return
        self._json_response(404, {"error": f"unknown path: {parsed.path}"})

    def do_PUT(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/fs/file":
            params = parse_qs(parsed.query)
            file_path = params.get("path", [None])[0]
            if not file_path:
                self._json_response(400, {"error": "missing path parameter"})
                return
            full_path = self._resolve_path(file_path)
            content_length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_length).decode() if content_length > 0 else ""
            try:
                os.makedirs(os.path.dirname(full_path), exist_ok=True)
                with open(full_path, "w") as f:
                    f.write(body)
                self._json_response(200, {"status": "ok", "path": file_path})
            except Exception as e:
                self._json_response(500, {"error": str(e)})
            return
        self._json_response(404, {"error": f"unknown path: {parsed.path}"})

    def do_POST(self):
        parsed = urlparse(self.path)
        if parsed.path == "/v1/processes/run":
            content_length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_length).decode() if content_length > 0 else "{}"
            try:
                req = json.loads(body)
            except json.JSONDecodeError as e:
                self._json_response(400, {"error": f"invalid JSON: {e}"})
                return
            command = req.get("command", "")
            cwd = req.get("workdir", self.workdir)
            if not command:
                self._json_response(400, {"error": "missing command"})
                return
            try:
                result = subprocess.run(
                    command,
                    shell=True,
                    capture_output=True,
                    text=True,
                    timeout=30,
                    cwd=cwd,
                )
                self._json_response(200, {
                    "stdout": result.stdout,
                    "stderr": result.stderr,
                    "exit_code": result.returncode,
                })
            except subprocess.TimeoutExpired:
                self._json_response(200, {
                    "stdout": "",
                    "stderr": "command timed out after 30s",
                    "exit_code": -1,
                })
            except Exception as e:
                self._json_response(500, {"error": str(e)})
            return
        self._json_response(404, {"error": f"unknown path: {parsed.path}"})

    def _resolve_path(self, path: str) -> str:
        if path.startswith("/"):
            return path
        return os.path.join(self.workdir, path)

    def _json_response(self, status: int, data: dict):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):
        sys.stderr.write(f"[sandbox] {args[0]} {args[1]} {args[2]}\n")


def main():
    parser = argparse.ArgumentParser(description="Local sandbox server for Temper Agent")
    parser.add_argument("--port", type=int, default=9999, help="Port to listen on")
    parser.add_argument("--workdir", default="/tmp/temper-sandbox", help="Working directory")
    args = parser.parse_args()

    os.makedirs(args.workdir, exist_ok=True)
    SandboxHandler.workdir = args.workdir

    server = HTTPServer(("0.0.0.0", args.port), SandboxHandler)
    print(f"Local sandbox server listening on http://localhost:{args.port}")
    print(f"Working directory: {args.workdir}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
        server.server_close()


if __name__ == "__main__":
    main()
