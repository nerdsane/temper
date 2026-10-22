#!/usr/bin/env python3
"""Mac-side incident filer for the Temper incident-rca app.

Reads one JSON object on stdin:
  {"mode": "TUNNEL_DOWN", "detected_at": "<iso>", "detail": "..."}
Dedupes against an open incident of the same failure mode, reopens a
Closed one on recurrence, otherwise creates a new Incident.
Authenticates with a freshly minted supervisor JWT (local issuer key).
Prints one outcome line. Runs on the Mac; the VM sensor calls it over SSH.
"""
import json
import subprocess
import sys
import urllib.request
import urllib.error

BASE = "http://127.0.0.1:3000"
NS = "Temper.IncidentRCA"
TENANT = "incident-rca"
OPEN_STATES = {"Detected", "Investigating", "Documented", "HypothesisStated",
               "FixProposed", "Verifying", "Reopened"}


def token():
    out = subprocess.run(["python3", "/Users/bencharney/temper-server/mint_token.py",
                          "supervisor", "astro"], capture_output=True, text=True, timeout=30)
    return out.stdout.strip()


def call(tok, method, path, body=None):
    h = {"Content-Type": "application/json", "X-Tenant-Id": TENANT,
         "Authorization": f"Bearer {tok}"}
    req = urllib.request.Request(BASE + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers=h)
    with urllib.request.urlopen(req, timeout=20) as r:
        raw = r.read().decode()
        return r.status, json.loads(raw) if raw else {}


def main():
    item = json.loads(sys.stdin.read())
    tok = token()
    _, listing = call(tok, "GET", "/tdata/Incidents")
    same_mode = [v for v in listing.get("value", [])
                 if v.get("fields", {}).get("FailureMode") == item["mode"]]
    for inc in same_mode:
        if inc.get("status") in OPEN_STATES:
            print(f"deduped against open incident {inc['entity_id']}")
            return 0
    for inc in same_mode:
        if inc.get("status") == "Closed":
            call(tok, "POST", f"/tdata/Incidents('{inc['entity_id']}')/{NS}.Reopen",
                 {"RecurrenceEvidence": f"{item['detected_at']} sensor: {item['detail']}"})
            print(f"reopened incident {inc['entity_id']}")
            return 0
    _, created = call(tok, "POST", "/tdata/Incidents", {
        "Title": f"Tailscale tunnel fault: {item['mode']}",
        "FailureMode": item["mode"],
        "SourceSignal": "tailnet-sensor (tailnet-diagnose.py via tunnel job failure path)",
        "DetectedAt": item["detected_at"],
    })
    print(f"opened incident {created.get('entity_id')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
