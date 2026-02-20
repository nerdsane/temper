"""Serve dashboard + proxy /tdata to Temper backend. One port, one tunnel."""
import http.server, urllib.request, urllib.error, json, os

TEMPER = "http://localhost:3001"
DASHBOARD = os.path.join(os.path.dirname(__file__), "dashboard.html")

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/" or self.path == "/dashboard":
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            with open(DASHBOARD, "rb") as f:
                self.wfile.write(f.read())
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

    def _proxy(self, method):
        url = TEMPER + self.path
        body = None
        if method in ("POST", "PATCH"):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else None

        req = urllib.request.Request(url, data=body, method=method)
        req.add_header("Content-Type", self.headers.get("Content-Type", "application/json"))

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
    http.server.HTTPServer(("0.0.0.0", port), Handler).serve_forever()
