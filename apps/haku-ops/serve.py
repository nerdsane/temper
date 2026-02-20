"""Haku Ops dashboard server.

Serves dashboard HTML and proxies /tdata to Temper backend.
Webhooks are handled natively by Temper (webhooks.toml) — no more Python polling.
"""
import http.server
import json
import os
import urllib.error
import urllib.request

TEMPER = "http://localhost:3001"
DIR = os.path.dirname(__file__)
DASHBOARD = os.path.join(DIR, "dashboard.html")


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/dashboard"):
            self._serve_dashboard()
        elif self.path.startswith("/tdata"):
            self._proxy("GET")
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path.startswith("/tdata"):
            self._proxy("POST")
        else:
            self.send_error(404)

    def do_PATCH(self):
        if self.path.startswith("/tdata"):
            self._proxy("PATCH")
        else:
            self.send_error(404)

    def do_OPTIONS(self):
        self.send_response(204)
        self._cors()
        self.send_header("Access-Control-Allow-Methods", "GET, POST, PATCH, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, X-Tenant-Id")
        self.end_headers()

    def _cors(self):
        self.send_header("Access-Control-Allow-Origin", "*")

    def _serve_dashboard(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Cache-Control", "no-cache")
        self._cors()
        self.end_headers()
        with open(DASHBOARD, "rb") as f:
            self.wfile.write(f.read())

    def _proxy(self, method):
        url = TEMPER + self.path
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None

        req = urllib.request.Request(url, data=body, method=method)
        req.add_header(
            "Content-Type",
            self.headers.get("Content-Type", "application/json")
        )
        tenant = self.headers.get("X-Tenant-Id")
        if tenant:
            req.add_header("X-Tenant-Id", tenant)

        try:
            with urllib.request.urlopen(req) as resp:
                data = resp.read()
                self.send_response(resp.status)
                self.send_header("Content-Type", "application/json")
                self._cors()
                self.end_headers()
                self.wfile.write(data)
        except urllib.error.HTTPError as e:
            self.send_response(e.code)
            self.send_header("Content-Type", "application/json")
            self._cors()
            self.end_headers()
            self.wfile.write(e.read())

    def log_message(self, fmt, *args):
        pass


if __name__ == "__main__":
    port = 8080
    print(f"Haku Ops Dashboard: http://localhost:{port}")
    print(f"Proxying /tdata → {TEMPER}/tdata")
    print(f"Webhooks: handled by Temper natively (webhooks.toml)")
    http.server.HTTPServer(("0.0.0.0", port), Handler).serve_forever()
