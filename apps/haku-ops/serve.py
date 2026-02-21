#!/usr/bin/env python3
"""Lightweight proxy: serves UI + forwards /tdata to Temper + watches for actions."""
import http.server, urllib.request, json, os, threading, time
import psycopg2

TEMPER = os.environ.get("TEMPER_URL", "http://localhost:3001")
PORT = int(os.environ.get("PORT", "8080"))
HTML_DIR = os.path.dirname(os.path.abspath(__file__))
DB_URL = os.environ.get("DATABASE_URL", "postgres://temper:temper_dev@localhost/haku_ops")
OPENCLAW_HOOK = "http://127.0.0.1:18789/hooks/wake"
OPENCLAW_TOKEN = "d43a532c3e1ef83771dbc77c45ff2992"
WAKE_ACTIONS = {"Select", "Deselect", "WritePlan", "Approve", "StartImplementation",
                "CompleteImplementation", "Scratch", "Verify", "AttachShowboat", "MarkCIPassed", "VerifyDeployment"}
POLL_INTERVAL = 5  # seconds

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

def watcher():
    """Poll trajectories table for new actions, wake OpenClaw on match."""
    last_seq = 0
    # Get current max to avoid firing on historical data
    try:
        conn = psycopg2.connect(DB_URL)
        conn.autocommit = True
        with conn.cursor() as cur:
            cur.execute("SELECT COALESCE(MAX(id), 0) FROM trajectories WHERE tenant = 'haku-ops'")
            last_seq = cur.fetchone()[0]
        conn.close()
        print(f"Watcher: starting from seq {last_seq}")
    except Exception as e:
        print(f"Watcher: init error: {e}")

    while True:
        time.sleep(POLL_INTERVAL)
        try:
            conn = psycopg2.connect(DB_URL)
            conn.autocommit = True
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT id, action, entity_id FROM trajectories "
                    "WHERE tenant = 'haku-ops' AND id > %s ORDER BY id",
                    (last_seq,)
                )
                rows = cur.fetchall()
            conn.close()

            for seq, action, eid in rows:
                last_seq = seq
                if action in WAKE_ACTIONS:
                    print(f"Watcher: {action} on {eid} (seq {seq}) → waking OpenClaw")
                    try:
                        body = json.dumps({"text": f"Temper action: {action} on {eid}"}).encode()
                        req = urllib.request.Request(
                            OPENCLAW_HOOK,
                            data=body,
                            headers={"Content-Type": "application/json", "Authorization": f"Bearer {OPENCLAW_TOKEN}"},
                            method="POST"
                        )
                        with urllib.request.urlopen(req, timeout=5) as r:
                            print(f"Watcher: wake returned {r.status}")
                    except Exception as e:
                        print(f"Watcher: wake failed: {e}")
        except Exception as e:
            print(f"Watcher: poll error: {e}")


if __name__ == "__main__":
    t = threading.Thread(target=watcher, daemon=True)
    t.start()
    print(f"Serving on :{PORT} → {TEMPER} (watcher active)")
    http.server.HTTPServer(("", PORT), Handler).serve_forever()
