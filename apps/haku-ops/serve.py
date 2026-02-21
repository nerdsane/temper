#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper."""
import http.server, urllib.request, json, os

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT = int(os.environ.get("PORT", "8080"))
HTML_DIR = os.path.dirname(os.path.abspath(__file__))

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/tdata"):
            self._proxy_stream("GET") if "$events" in self.path else self._proxy("GET")
        elif self.path == "/temper-client.js" or self.path == "/static/temper-client.js":
            self._proxy_static(self.path)
        else:
            self._serve_html()

    def do_POST(self):
        self._proxy("POST")

    def do_PATCH(self):
        self._proxy("PATCH")

    def do_OPTIONS(self):
        self.send_response(204)
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET,POST,PATCH,OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type,X-Tenant-Id")
        self.end_headers()

    def _serve_html(self):
        path = os.path.join(HTML_DIR, "index.html")
        with open(path, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(data)

    def _proxy_static(self, path):
        """Proxy static files from Temper (e.g. temper-client.js)."""
        url = f"{TEMPER}{path}"
        try:
            with urllib.request.urlopen(url) as resp:
                data = resp.read()
                self.send_response(200)
                ct = resp.headers.get("Content-Type", "application/javascript")
                self.send_header("Content-Type", ct)
                self.send_header("Cache-Control", "public, max-age=60")
                self.end_headers()
                self.wfile.write(data)
        except Exception:
            self.send_response(502)
            self.end_headers()

    def _proxy_stream(self, method):
        """Proxy SSE stream from Temper with chunked transfer."""
        url = f"{TEMPER}{self.path}"
        headers = {"X-Tenant-Id": "haku-ops", "Accept": "text/event-stream"}
        req = urllib.request.Request(url, headers=headers, method=method)
        try:
            resp = urllib.request.urlopen(req)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            while True:
                chunk = resp.read(4096)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except Exception:
            pass

    def _proxy(self, method):
        url = f"{TEMPER}{self.path}"
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None
        headers = {"Content-Type": "application/json", "X-Tenant-Id": "haku-ops"}
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
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(e.read())

    def log_message(self, fmt, *args):
        print(fmt % args)

if __name__ == "__main__":
    print(f"Serving on :{PORT} → {TEMPER}")
    http.server.HTTPServer(("", PORT), Handler).serve_forever()
