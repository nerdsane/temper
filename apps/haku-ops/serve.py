"""Haku Ops dashboard server.

Serves the dashboard HTML, proxies /tdata to Temper backend,
queries trajectories for live event feed, and watches for
Rita's actions to wake Haku via OpenClaw webhook.

No polling crons. No wasted inference. Python watches Postgres
(cheap), only calls OpenClaw when something actually happens.
"""
import http.server
import json
import os
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

import psycopg2

TEMPER = "http://localhost:3001"
DIR = os.path.dirname(__file__)
DASHBOARD = os.path.join(DIR, "dashboard.html")
TENANT = "haku-ops"

# Postgres
DB_DSN = os.environ.get(
    "TEMPER_DB_DSN",
    "dbname=haku_ops user=temper password=temper_dev host=localhost"
)

# OpenClaw webhook — wake Haku when Rita acts
OPENCLAW_HOOK_URL = os.environ.get(
    "OPENCLAW_HOOK_URL",
    "http://127.0.0.1:18789/hooks/wake"
)
OPENCLAW_HOOK_TOKEN = os.environ.get(
    "OPENCLAW_HOOK_TOKEN",
    "d43a532c3e1ef83771dbc77c45ff2992"
)

# Watch interval (seconds) — how often to poll Postgres for new actions
WATCH_INTERVAL = 5

# Actions that should wake Haku
WAKE_ACTIONS = {"Select", "Deselect", "Approve", "Scratch", "WritePlan"}


def get_db():
    return psycopg2.connect(DB_DSN)


def wake_haku(action, entity_type, entity_id, from_status, to_status):
    """POST to OpenClaw /hooks/wake to inject event into Haku's main session."""
    payload = json.dumps({
        "text": (
            f"[Temper] Rita fired '{action}' on {entity_type} '{entity_id}' "
            f"({from_status} → {to_status}). "
            f"Check state: curl -s http://localhost:3001/tdata/{entity_type}s('{entity_id}') "
            f"-H 'X-Tenant-Id: haku-ops' | python3 -m json.tool"
        ),
        "mode": "now"
    }).encode()

    req = urllib.request.Request(
        OPENCLAW_HOOK_URL,
        data=payload,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {OPENCLAW_HOOK_TOKEN}"
        }
    )
    try:
        with urllib.request.urlopen(req) as resp:
            print(f"[watch] Woke Haku: {action} on {entity_id} → {resp.status}")
    except Exception as e:
        print(f"[watch] Failed to wake Haku: {e}")


def watcher_loop():
    """Background thread: poll trajectories table, wake Haku on new actions."""
    last_id = 0

    # Initialize: get the latest trajectory ID so we don't replay history
    try:
        conn = get_db()
        cur = conn.cursor()
        cur.execute(
            "SELECT COALESCE(MAX(id), 0) FROM trajectories WHERE tenant = %s",
            (TENANT,)
        )
        last_id = cur.fetchone()[0]
        cur.close()
        conn.close()
        print(f"[watch] Initialized at trajectory id {last_id}")
    except Exception as e:
        print(f"[watch] Init failed: {e}")

    while True:
        time.sleep(WATCH_INTERVAL)
        try:
            conn = get_db()
            cur = conn.cursor()
            cur.execute(
                """SELECT id, entity_type, entity_id, action, from_status, to_status
                   FROM trajectories
                   WHERE tenant = %s AND id > %s AND success = true
                   ORDER BY id ASC LIMIT 20""",
                (TENANT, last_id)
            )
            rows = cur.fetchall()
            cur.close()
            conn.close()

            for row in rows:
                tid, etype, eid, action, from_s, to_s = row
                last_id = tid
                if action in WAKE_ACTIONS:
                    print(f"[watch] New action: {action} on {etype}/{eid}")
                    wake_haku(action, etype, eid, from_s or "—", to_s or "—")

        except Exception as e:
            print(f"[watch] Poll error: {e}")
            time.sleep(10)  # backoff on error


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

    # Start the background watcher
    watcher = threading.Thread(target=watcher_loop, daemon=True)
    watcher.start()

    print(f"Haku Ops Dashboard: http://localhost:{port}")
    print(f"Proxying /tdata → {TEMPER}/tdata")
    print(f"Watching trajectories every {WATCH_INTERVAL}s → OpenClaw webhook")
    print(f"Tenant: {TENANT}")
    http.server.HTTPServer(("0.0.0.0", port), Handler).serve_forever()
