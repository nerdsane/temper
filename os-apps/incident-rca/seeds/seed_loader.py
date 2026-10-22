#!/usr/bin/env python3
"""Load the historical tunnel-incident seed corpus into the Temper
incident-rca app (tenant incident-rca, Mac-local API), walking each seed
through the governed workflow to Closed with its recorded evidence.
Idempotent: skips seeds whose Title already exists.

Auth: fresh supervisor JWT minted per run by ~/temper-server/mint_token.py.
Run ON the Mac:  python3 seed_loader.py
"""
import json
import os
import subprocess
import sys
import urllib.request
import urllib.error

BASE = "http://127.0.0.1:3000"
NS = "Temper.IncidentRCA"
TENANT = "incident-rca"


def token():
    out = subprocess.run(["python3", "/Users/bencharney/temper-server/mint_token.py",
                          "supervisor", "astro"], capture_output=True, text=True, timeout=30)
    return out.stdout.strip()


TOK = token()


def call(method, path, body=None):
    h = {"Content-Type": "application/json", "Accept": "application/json",
         "X-Tenant-Id": TENANT, "Authorization": f"Bearer {TOK}"}
    req = urllib.request.Request(BASE + path, method=method,
                                 data=json.dumps(body).encode() if body is not None else None,
                                 headers=h)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read().decode()
            return r.status, json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        return e.code, {"error": e.read().decode()[:400]}


def act(entity_id, action, params):
    status, resp = call("POST", f"/tdata/Incidents('{entity_id}')/{NS}.{action}", params)
    if status not in (200, 201):
        print(f"  ! {action} -> {status}: {resp}")
        return False
    return True


def main():
    seeds_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tunnel-incidents.json")
    seeds = json.load(open(seeds_path))
    status, existing = call("GET", "/tdata/Incidents")
    if status != 200:
        print(f"cannot list incidents ({status}): {existing}")
        return 1
    existing_titles = {v.get("fields", {}).get("Title") for v in existing.get("value", [])}

    for seed in seeds:
        if seed["title"] in existing_titles:
            print(f"skip (exists): {seed['seed_id']}")
            continue
        body = {
            "Title": seed["title"],
            "FailureMode": seed["failure_mode"],
            "SourceSignal": seed["source_signal"],
            "DetectedAt": seed["detected_at"],
            "RootCauseHypothesis": seed["root_cause_hypothesis"],
            "WhyPriorFixesFailed": seed["why_prior_fixes_failed"],
        }
        status, created = call("POST", "/tdata/Incidents", body)
        if status not in (200, 201):
            print(f"FAIL create {seed['seed_id']}: {status} {created}")
            continue
        eid = created.get("entity_id")
        print(f"created {seed['seed_id']} -> {eid}")
        evidence = "; ".join(seed["evidence_refs"])
        ok = True
        ok &= act(eid, "BeginInvestigation", {"InvestigatorId": "astro (seed load)"})
        ok &= act(eid, "AttachEvidence", {"EvidenceRef": evidence})
        ok &= act(eid, "RecordPriorReview", {"PriorIncidentRefs": "seed corpus load — priors are the earlier seeds in tunnel-incidents.json"})
        ok &= act(eid, "CompleteDocumentation", {})
        ok &= act(eid, "StateHypothesis", {"RootCauseHypothesis": seed["root_cause_hypothesis"], "WhyPriorFixesFailed": seed["why_prior_fixes_failed"]})
        ok &= act(eid, "ProposeFix", {})
        ok &= act(eid, "BeginVerification", {"RemediationRef": "historical — recovery observed, no Temper remediation entity"})
        ok &= act(eid, "RecordVerification", {"VerificationEvidence": seed["verification_evidence"]})
        ok &= act(eid, "Close", {})
        print(f"  walked to Closed: {'OK' if ok else 'PARTIAL — see above'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
