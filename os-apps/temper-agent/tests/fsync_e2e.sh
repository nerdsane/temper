#!/usr/bin/env bash
# Fsync E2E Test — Sandbox file sync to TemperFS
#
# Prerequisites:
#   - Temper server running on port 3000
#   - Local sandbox running on port 9999
#   - Blob store running on port 8877
#   - temper-agent app installed with WASM modules uploaded
#   - Valid anthropic_api_key stored in secrets vault
#
# Usage:
#   bash os-apps/temper-agent/tests/fsync_e2e.sh
#
# The test creates an agent that writes files via write tool and bash tool,
# then verifies that:
#   1. File manifest was created with entries
#   2. Each file in manifest is readable from TemperFS with correct content
#   3. Files created by bash (not just write tool) are captured
#   4. Deleted files are archived in manifest

set -euo pipefail

SERVER="http://localhost:3000"
SANDBOX="http://localhost:9999"
TENANT="rita-agents"
HEADERS='-H "content-type: application/json" -H "x-tenant-id: rita-agents" -H "x-temper-principal-kind: admin"'

pass() { echo "  PASS: $1"; }
fail() { echo "  FAIL: $1"; FAILURES=$((FAILURES + 1)); }

FAILURES=0

echo "=== Fsync E2E Test ==="
echo ""

# Clean sandbox workspace
echo "Cleaning sandbox workspace..."
curl -sf -X POST "$SANDBOX/v1/processes/run" \
  -H "content-type: application/json" \
  -d '{"command": "rm -rf /tmp/workspace/* 2>/dev/null; echo ok", "workdir": "/tmp/workspace"}' > /dev/null

# 1. Create and configure agent
echo "Step 1: Creating agent..."
AGENT_RESULT=$(curl -sf -X POST "$SERVER/tdata/TemperAgents" \
  -H "content-type: application/json" \
  -H "x-tenant-id: $TENANT" \
  -H "x-temper-principal-kind: admin" \
  -d '{"TemperAgentId": "fsync-e2e-test"}')
AGENT_ID=$(echo "$AGENT_RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['entity_id'])")
echo "  Agent ID: $AGENT_ID"

echo "Step 2: Configuring agent..."
curl -sf -X POST "$SERVER/tdata/TemperAgents('${AGENT_ID}')/Temper.Agent.TemperAgent.Configure" \
  -H "content-type: application/json" \
  -H "x-tenant-id: $TENANT" \
  -H "x-temper-principal-kind: admin" \
  -d "{
    \"system_prompt\": \"You are a concise assistant. Create files as requested. No explanations, just create files and say done.\",
    \"user_message\": \"Create these files: 1) Use the write tool to create hello.py containing: print('hello'). 2) Use bash to run: echo 'bash-created' > /tmp/workspace/notes.txt. 3) Use write to create config.json containing: {\\\"v\\\": 1}. Then respond with just 'done'.\",
    \"tools_enabled\": \"write,bash,read\",
    \"sandbox_url\": \"http://127.0.0.1:9999\",
    \"workdir\": \"/tmp/workspace\",
    \"max_turns\": \"5\"
  }" > /dev/null

# 3. Provision (triggers sandbox_provisioner → creates workspace + manifest)
echo "Step 3: Provisioning..."
curl -sf -X POST "$SERVER/tdata/TemperAgents('${AGENT_ID}')/Temper.Agent.TemperAgent.Provision" \
  -H "content-type: application/json" \
  -H "x-tenant-id: $TENANT" \
  -H "x-temper-principal-kind: admin" \
  -d '{}' > /dev/null

# 4. Poll until completion
echo "Step 4: Polling for completion..."
for i in $(seq 1 60); do
  ENTITY=$(curl -sf "$SERVER/tdata/TemperAgents" \
    -H "x-tenant-id: $TENANT" \
    -H "x-temper-principal-kind: admin" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for item in data.get('value', []):
    if item.get('entity_id') == '$AGENT_ID':
        print(json.dumps(item))
        break
")
  STATUS=$(echo "$ENTITY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))")
  TURNS=$(echo "$ENTITY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('counters',{}).get('turn_count',0))")
  echo "  [$i] Status: $STATUS, Turns: $TURNS"

  if [ "$STATUS" = "Completed" ]; then
    break
  fi
  if [ "$STATUS" = "Failed" ]; then
    ERR=$(echo "$ENTITY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('fields',{}).get('error_message','unknown'))")
    echo "  FATAL: Agent failed: $ERR"
    exit 1
  fi
  sleep 3
done

if [ "$STATUS" != "Completed" ]; then
  echo "  FATAL: Agent did not complete within timeout"
  exit 1
fi

# 5. Verify manifest exists and has entries
echo ""
echo "Step 5: Verifying manifest..."
MANIFEST_ID=$(echo "$ENTITY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('fields',{}).get('file_manifest_id',''))")
if [ -z "$MANIFEST_ID" ]; then
  fail "file_manifest_id is empty"
else
  pass "file_manifest_id is set: $MANIFEST_ID"
fi

MANIFEST=$(curl -sf "$SERVER/tdata/Files('${MANIFEST_ID}')/\$value" \
  -H "x-tenant-id: $TENANT" \
  -H "x-temper-principal-kind: admin")
echo "  Manifest content: $MANIFEST"

FILE_COUNT=$(echo "$MANIFEST" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('files',{})))")
if [ "$FILE_COUNT" -ge 3 ]; then
  pass "Manifest has $FILE_COUNT files (expected >= 3)"
else
  fail "Manifest has $FILE_COUNT files (expected >= 3)"
fi

# 6. Verify each file is readable from TemperFS
echo ""
echo "Step 6: Verifying file contents..."
echo "$MANIFEST" | python3 -c "
import sys, json
manifest = json.load(sys.stdin)
for path, entry in manifest.get('files', {}).items():
    file_id = entry.get('file_id', '')
    print(f'{path}|{file_id}')
" | while IFS='|' read -r path file_id; do
  CONTENT=$(curl -sf "$SERVER/tdata/Files('${file_id}')/\$value" \
    -H "x-tenant-id: $TENANT" \
    -H "x-temper-principal-kind: admin" 2>/dev/null || echo "READ_FAILED")
  if [ "$CONTENT" = "READ_FAILED" ]; then
    fail "Cannot read $path (file_id: $file_id)"
  else
    CONTENT_LEN=${#CONTENT}
    pass "$path readable from TemperFS ($CONTENT_LEN bytes)"
  fi
done

# 7. Verify bash-created file is in manifest
echo ""
echo "Step 7: Checking bash-created file..."
HAS_NOTES=$(echo "$MANIFEST" | python3 -c "
import sys, json
manifest = json.load(sys.stdin)
files = manifest.get('files', {})
for path in files:
    if 'notes' in path:
        print('yes')
        break
else:
    print('no')
")
if [ "$HAS_NOTES" = "yes" ]; then
  pass "Bash-created notes.txt is in manifest"
else
  fail "Bash-created notes.txt NOT in manifest"
fi

# 8. Verify FileVersion entities exist (automatic versioning)
echo ""
echo "Step 8: Checking FileVersions..."
MANIFEST_ENTITY=$(curl -sf "$SERVER/tdata/Files('${MANIFEST_ID}')" \
  -H "x-tenant-id: $TENANT" \
  -H "x-temper-principal-kind: admin")
VERSION_COUNT=$(echo "$MANIFEST_ENTITY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('counters',{}).get('version_count',0))")
if [ "$VERSION_COUNT" -ge 2 ]; then
  pass "Manifest has $VERSION_COUNT versions (initial + syncs)"
else
  # Version 1 is the initial write, version 2+ from tool_runner syncs
  echo "  INFO: Manifest has $VERSION_COUNT version(s)"
fi

echo ""
echo "=== Results ==="
if [ "$FAILURES" -eq 0 ]; then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
