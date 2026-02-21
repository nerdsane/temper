#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper, including SSE."""
import http.server, urllib.request, json, os

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT = int(os.environ.get("PORT", "8091"))
HTML = os.path.join(os.path.dirname(__file__), "index.html")
TENANT = os.environ.get("TENANT", "agent-loop")

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/tdata"):
            if "$events" in self.path:
                self._proxy_sse()
            else:
                self._proxy("GET")
        else:
            self._serve_html()

    def do_POST(self):
        self._proxy("POST")

    def do_PATCH(self):
        self._proxy("PATCH")

    def _serve_html(self):
        with open(HTML, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(data)

    def _proxy_sse(self):
        """Stream SSE events from Temper to the client."""
        url = f"{TEMPER}{self.path}"
        headers = {"Accept": "text/event-stream", "X-Tenant-Id": TENANT}
        req = urllib.request.Request(url, headers=headers)
        try:
            resp = urllib.request.urlopen(req, timeout=300)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "keep-alive")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            while True:
                line = resp.readline()
                if not line:
                    break
                self.wfile.write(line)
                self.wfile.flush()
        except Exception:
            self.send_response(502)
            self.end_headers()

    def _proxy(self, method):
        url = f"{TEMPER}{self.path}"
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None
        headers = {"Content-Type": "application/json", "X-Tenant-Id": TENANT}
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req) as resp:
                data = resp.read()
                self.send_response(resp.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Access-Control-Allow-Origin", "*")
                self.end_headers()
                self.wfile.write(data)
        except urllib.error.HTTPError as e:
            self.send_response(e.code)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(e.read())

    def log_message(self, fmt, *args):
        pass

if __name__ == "__main__":
    print(f"Agent Loop demo on :{PORT} → {TEMPER}")
    http.server.HTTPServer(("", PORT), Handler).serve_forever()
