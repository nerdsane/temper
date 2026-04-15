#!/usr/bin/env python3
"""Minimal local blob storage server for TemperFS development.

Implements a content-addressable store compatible with the blob_adapter WASM module:
  PUT /{bucket}/{hash}   → store blob
  GET /{bucket}/{hash}   → retrieve blob
  HEAD /{bucket}/{hash}  → check existence
  GET /health            → health check

Blobs are stored as files in a local directory keyed by their hash.

Usage:
  python3 local_blob_store.py [--port 8877] [--dir /tmp/temper-blobs]
"""

import argparse
import json
import os
import sys
from http.server import HTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse


class BlobHandler(BaseHTTPRequestHandler):
    blob_dir: str = "/tmp/temper-blobs"

    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._json_response(200, {"status": "ok"})
            return
        blob_path = self._blob_path(parsed.path)
        if not blob_path:
            self._json_response(400, {"error": "invalid path"})
            return
        if not os.path.isfile(blob_path):
            self._json_response(404, {"error": "blob not found"})
            return
        with open(blob_path, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_HEAD(self):
        parsed = urlparse(self.path)
        blob_path = self._blob_path(parsed.path)
        if not blob_path or not os.path.isfile(blob_path):
            self.send_response(404)
            self.end_headers()
            return
        size = os.path.getsize(blob_path)
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.end_headers()

    def do_PUT(self):
        parsed = urlparse(self.path)
        blob_path = self._blob_path(parsed.path)
        if not blob_path:
            self._json_response(400, {"error": "invalid path"})
            return
        content_length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(content_length) if content_length > 0 else b""
        os.makedirs(os.path.dirname(blob_path), exist_ok=True)
        with open(blob_path, "wb") as f:
            f.write(body)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def _blob_path(self, url_path: str) -> str | None:
        parts = url_path.strip("/").split("/")
        if len(parts) < 2:
            return None
        bucket = parts[0]
        blob_hash = parts[1]
        return os.path.join(self.blob_dir, bucket, blob_hash)

    def _json_response(self, status: int, data: dict):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        sys.stderr.write(f"[blob] {args[0]} {args[1]} {args[2]}\n")


def main():
    parser = argparse.ArgumentParser(description="Local blob storage for TemperFS")
    parser.add_argument("--port", type=int, default=8877, help="Port to listen on")
    parser.add_argument("--dir", default="/tmp/temper-blobs", help="Blob storage directory")
    args = parser.parse_args()

    os.makedirs(args.dir, exist_ok=True)
    BlobHandler.blob_dir = args.dir

    server = HTTPServer(("0.0.0.0", args.port), BlobHandler)
    print(f"Local blob store listening on http://localhost:{args.port}")
    print(f"Storage directory: {args.dir}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\nShutting down.")
        server.server_close()


if __name__ == "__main__":
    main()
