#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper."""
import http.server, urllib.request, json, os

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT = int(os.environ.get("PORT", "8080"))
HTML_DIR = os.path.dirname(os.path.abspath(__file__))

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.startswith("/tdata"):
            self._proxy("GET")
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
        pass

if __name__ == "__main__":
    print(f"Serving on :{PORT} → {TEMPER}")
    http.server.HTTPServer(("", PORT), Handler).serve_forever()
