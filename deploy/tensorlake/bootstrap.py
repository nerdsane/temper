"""Run inside a fresh sandbox to grant its operator Observe read access."""
import json
from pathlib import Path
import shlex
import time
import urllib.request

values = dict(line.split("=", 1) for line in Path("/etc/temper/runtime.env").read_text().splitlines())
key = shlex.split(values["TEMPER_API_KEY"])[0]
for attempt in range(60):
    try:
        with urllib.request.urlopen("http://127.0.0.1:3000/healthz", timeout=2) as response:
            assert response.status == 200
        break
    except (OSError, AssertionError):
        if attempt == 59:
            raise
        time.sleep(1)
# No application mutation permissions. The kernel seeds manage_policies separately.
actions = ["read", "list", "read_entities", "read_events", "read_agents", "read_specs", "read_wasm", "read_evolution", "read_trajectories", "read_verification"]
action_set = ", ".join('Action::"' + action + '"' for action in actions)
policy = {
    "policy_id": "tensorlake-observe-read",
    "cedar_text": 'permit(principal == Agent::"operator", action in [' + action_set + '], resource) when { principal has agent_type && principal.agent_type == "operator" && principal has agentTypeVerified && principal.agentTypeVerified == true };',
}
request = urllib.request.Request(
    "http://127.0.0.1:3000/api/tenants/default/policies/create",
    data=json.dumps(policy).encode(),
    headers={"Authorization": "Bearer " + key, "X-Tenant-Id": "default", "Content-Type": "application/json"},
)
with urllib.request.urlopen(request, timeout=30) as response:
    assert response.status == 201
print("Configured authenticated Observe read access within the default tenant.")
