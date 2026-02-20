"""Serve dashboard + proxy /tdata to Temper backend + selection bridge."""
import http.server, urllib.request, urllib.error, json, os, time

TEMPER = "http://localhost:3001"
DIR = os.path.dirname(__file__)
DASHBOARD = os.path.join(DIR, "dashboard.html")
SELECTION_FILE = os.path.join(DIR, ".selection.json")

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/" or self.path == "/dashboard":
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            with open(DASHBOARD, "rb") as f:
                self.wfile.write(f.read())
        elif self.path == "/selection":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            try:
                with open(SELECTION_FILE) as f:
                    self.wfile.write(f.read().encode())
            except FileNotFoundError:
                self.wfile.write(b'{"selected":null}')
        elif self.path.startswith("/tdata"):
            self._proxy("GET")
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path == "/selection":
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else b'{}'
            data = json.loads(body)
            data["timestamp"] = time.time()
            with open(SELECTION_FILE, "w") as f:
                json.dump(data, f)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            self.wfile.write(json.dumps({"ok": True}).encode())
        elif self.path.startswith("/tdata"):
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
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, PATCH, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "Content-Type, X-Tenant-Id")
        self.end_headers()

    def _proxy(self, method):
        url = TEMPER + self.path
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None

        req = urllib.request.Request(url, data=body, method=method)
        req.add_header("Content-Type", self.headers.get("Content-Type", "application/json"))
        tenant = self.headers.get("X-Tenant-Id")
        if tenant:
            req.add_header("X-Tenant-Id", tenant)

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
        pass  # quiet

if __name__ == "__main__":
    port = 8080
    print(f"Dashboard: http://localhost:{port}")
    print(f"Proxying /tdata → {TEMPER}/tdata")
    print(f"Selection file: {SELECTION_FILE}")
    http.server.HTTPServer(("0.0.0.0", port), Handler).serve_forever()
