#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper.

SSE-aware: EventSource connections (/tdata/$events) are streamed through
chunk-by-chunk so real-time updates reach the browser. All other requests
are buffered normally.

Usage:
    python3 serve.py            # serves index.html on port 8080
    PORT=9090 python3 serve.py  # different port
    TEMPER_URL=http://localhost:3002 python3 serve.py  # different Temper instance

Copy this file to your app directory alongside index.html:
    cp ~/workspace/Development/temper/skills/temper/serve.py ~/workspace/apps/my-app/serve.py
"""
import http.server, urllib.request, urllib.error, os
from socketserver import ThreadingMixIn

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT   = int(os.environ.get("PORT", "8080"))
HTML   = os.path.join(os.path.dirname(os.path.abspath(__file__)), "index.html")


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith(("/tdata", "/temper-client.js")):
            self._proxy("GET")
        else:
            self._serve_html()

    def do_POST(self):  self._proxy("POST")
    def do_PATCH(self): self._proxy("PATCH")

    def _serve_html(self):
        try:
            with open(HTML, "rb") as f:
                data = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(data)
        except FileNotFoundError:
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"index.html not found")

    def _proxy(self, method):
        url  = f"{TEMPER}{self.path}"
        body = None
        if method in ("POST", "PATCH"):
            n    = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(n) if n else None
        hdrs = {"Content-Type": "application/json"}
        if t := self.headers.get("X-Tenant-Id"):
            hdrs["X-Tenant-Id"] = t
        req = urllib.request.Request(url, data=body, headers=hdrs, method=method)
        try:
            with urllib.request.urlopen(req) as resp:
                ct = resp.headers.get("Content-Type", "")
                self.send_response(resp.status)
                self.send_header("Content-Type", ct)
                self.send_header("Access-Control-Allow-Origin", "*")
                if "text/event-stream" in ct:
                    # SSE: must stream — never buffer. urlopen reads lazily per-chunk.
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("X-Accel-Buffering", "no")
                self.end_headers()
                if "text/event-stream" in ct:
                    # Stream SSE lines as they arrive — flush after every write
                    try:
                        while chunk := resp.read(256):
                            self.wfile.write(chunk)
                            self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        pass  # client disconnected — normal SSE lifecycle
                else:
                    self.wfile.write(resp.read())
        except urllib.error.HTTPError as e:
            self.send_response(e.code)
            self.end_headers()
            self.wfile.write(e.read())
        except urllib.error.URLError:
            self.send_response(502)
            self.end_headers()

    def log_message(self, *_): pass


class ThreadedServer(ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True  # SO_REUSEADDR — no "address already in use" on restart


if __name__ == "__main__":
    s = ThreadedServer(("0.0.0.0", PORT), Handler)  # 0.0.0.0 = LAN accessible
    print(f"Serving :{PORT} → {TEMPER}")
    s.serve_forever()
