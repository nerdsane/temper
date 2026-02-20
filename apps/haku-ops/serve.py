"""Haku Ops dashboard server.

Serves the dashboard HTML, proxies /tdata to Temper backend,
and provides /trajectories endpoint for live event watching.
No more side-channel files — everything goes through Temper.
"""
import http.server
import json
import os
import urllib.error
import urllib.request

import psycopg2

TEMPER = "http://localhost:3001"
DIR = os.path.dirname(__file__)
DASHBOARD = os.path.join(DIR, "dashboard.html")
TENANT = "haku-ops"

# Postgres connection for direct event queries
DB_DSN = os.environ.get(
    "TEMPER_DB_DSN",
    "dbname=haku_ops user=temper password=temper_dev host=localhost"
)


def get_db():
    """Get a Postgres connection (short-lived per request)."""
    return psycopg2.connect(DB_DSN)


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path in ("/", "/dashboard"):
            self._serve_dashboard()
        elif self.path.startswith("/trajectories"):
            self._serve_trajectories()
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

    def _serve_trajectories(self):
        """Query trajectories table for recent actions in this tenant.

        Supports ?since=<ISO timestamp>&limit=N
        """
        from urllib.parse import parse_qs, urlparse
        parsed = urlparse(self.path)
        params = parse_qs(parsed.query)
        since = params.get("since", [None])[0]
        limit = int(params.get("limit", ["50"])[0])

        try:
            conn = get_db()
            cur = conn.cursor()
            if since:
                cur.execute(
                    """SELECT id, entity_type, entity_id, action, success,
                              from_status, to_status, error, created_at
                       FROM trajectories
                       WHERE tenant = %s AND created_at > %s
                       ORDER BY created_at DESC LIMIT %s""",
                    (TENANT, since, limit)
                )
            else:
                cur.execute(
                    """SELECT id, entity_type, entity_id, action, success,
                              from_status, to_status, error, created_at
                       FROM trajectories
                       WHERE tenant = %s
                       ORDER BY created_at DESC LIMIT %s""",
                    (TENANT, limit)
                )

            rows = cur.fetchall()
            cols = [d[0] for d in cur.description]
            events = []
            for row in rows:
                evt = dict(zip(cols, row))
                evt["created_at"] = evt["created_at"].isoformat()
                events.append(evt)

            cur.close()
            conn.close()

            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self._cors()
            self.end_headers()
            self.wfile.write(json.dumps({"value": events}).encode())
        except Exception as e:
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self._cors()
            self.end_headers()
            self.wfile.write(json.dumps({"error": str(e)}).encode())

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
        pass  # quiet


if __name__ == "__main__":
    port = 8080
    print(f"Haku Ops Dashboard: http://localhost:{port}")
    print(f"Proxying /tdata → {TEMPER}/tdata")
    print(f"Trajectories: /trajectories?since=<ISO>&limit=N")
    print(f"Tenant: {TENANT}")
    http.server.HTTPServer(("0.0.0.0", port), Handler).serve_forever()
