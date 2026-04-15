#!/usr/bin/env bash
# Temper Agent OS — Governance Demo
#
# Prerequisites:
#   1. Start the server:
#      DATABASE_URL=postgres://localhost:5432/temper \
#        cargo run -p temper-cli -- serve --port 3333 \
#        --app ecommerce=reference-apps/ecommerce/specs
#
#   2. Start the observe dashboard:
#      cd observe && npm run dev
#
#   3. Open http://localhost:3000 in your browser
#
# Then run this script to see the full agent governance loop.

set -euo pipefail

API="http://localhost:3333"
TENANT="ecommerce"

echo "========================================="
echo "  TEMPER AGENT OS — GOVERNANCE DEMO"
echo "========================================="
echo ""

# Verify server is running
if ! curl -s "$API/tdata" > /dev/null 2>&1; then
  echo "ERROR: Server not running on $API"
  echo "Start it with: DATABASE_URL=postgres://localhost:5432/temper cargo run -p temper-cli -- serve --port 3333 --app ecommerce=reference-apps/ecommerce/specs"
  exit 1
fi

# Step 1: Enable Cedar default-deny for the tenant
echo "Step 1: Enable Cedar default-deny"
curl -s -X PUT "$API/api/tenants/$TENANT/policies" \
  -H "Content-Type: application/json" \
  -d "{\"policy_text\": \"// Default deny — no permits\\n\"}" > /dev/null
echo "  Cedar policies loaded (empty = default-deny for agents)"
echo ""

# Step 2: Create an Order as an agent
echo "Step 2: Create Order as agent 'checkout-bot'"
RESULT=$(curl -s -X POST "$API/tdata/Orders" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: $TENANT" \
  -H "X-Temper-Principal-Kind: agent" \
  -H "X-Temper-Principal-Id: checkout-bot" \
  -d '{}')
ORDER_ID=$(echo "$RESULT" | python3 -c "import sys,json; print(json.load(sys.stdin)['entity_id'])")
echo "  Created: Orders('$ORDER_ID') in Draft"
echo ""

# Step 3: Agent tries a bound action — gets DENIED
echo "Step 3: Agent tries AddItem → DENIED (no matching permit policy)"
HTTP_CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  -X POST "$API/tdata/Orders('${ORDER_ID}')/Temper.AddItem" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: $TENANT" \
  -H "X-Temper-Principal-Kind: agent" \
  -H "X-Temper-Principal-Id: checkout-bot" \
  -d '{}')
echo "  HTTP $HTTP_CODE — Authorization denied"
echo "  → Check the Decisions page in the dashboard!"
echo ""

# Step 4: Show the pending decision
echo "Step 4: Pending Decision created"
DECISIONS=$(curl -s "$API/api/tenants/$TENANT/decisions")
PD_ID=$(echo "$DECISIONS" | python3 -c "
import sys,json
d = json.load(sys.stdin)['decisions']
pending = [x for x in d if x['status'] == 'pending']
if pending:
    p = pending[-1]
    print(p['id'])
" 2>/dev/null)
echo "$DECISIONS" | python3 -c "
import sys,json
d = json.load(sys.stdin)['decisions']
pending = [x for x in d if x['status'] == 'pending']
if pending:
    p = pending[-1]
    print(f\"  ID:     {p['id']}\")
    print(f\"  Agent:  {p['agent_id']}\")
    print(f\"  Action: {p['action']} on {p['resource_type']}\")
    print(f\"  Reason: {p['denial_reason']}\")
" 2>/dev/null
echo ""

# Step 5: Human approves with broad scope
echo "Step 5: Human approves with BROAD scope"
APPROVE=$(curl -s -X POST "$API/api/tenants/$TENANT/decisions/${PD_ID}/approve" \
  -H "Content-Type: application/json" \
  -d '{"scope": "broad", "decided_by": "admin"}')
echo "$APPROVE" | python3 -c "
import sys,json
r = json.load(sys.stdin)
print(f\"  Status: {r['status']}\")
print(f\"  Generated Cedar policy:\")
for line in r['generated_policy'].split('\\n'):
    print(f\"    {line}\")
" 2>/dev/null
echo ""

# Step 6: Agent retries — now succeeds
echo "Step 6: Agent retries AddItem → SUCCESS"
curl -s -X POST "$API/tdata/Orders('${ORDER_ID}')/Temper.AddItem" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: $TENANT" \
  -H "X-Temper-Principal-Kind: agent" \
  -H "X-Temper-Principal-Id: checkout-bot" \
  -d '{}' | python3 -c "
import sys,json
r = json.load(sys.stdin)
print(f\"  Status: {r['status']}, Items: {r['item_count']}\")
" 2>/dev/null
echo ""

# Step 7: Continue the lifecycle
echo "Step 7: SubmitOrder → SUCCESS (broad scope covers all Order actions)"
curl -s -X POST "$API/tdata/Orders('${ORDER_ID}')/Temper.SubmitOrder" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: $TENANT" \
  -H "X-Temper-Principal-Kind: agent" \
  -H "X-Temper-Principal-Id: checkout-bot" \
  -d '{}' | python3 -c "
import sys,json
r = json.load(sys.stdin)
print(f\"  Status: {r['status']}\")
" 2>/dev/null
echo ""

echo "Step 8: ConfirmOrder → SUCCESS"
curl -s -X POST "$API/tdata/Orders('${ORDER_ID}')/Temper.ConfirmOrder" \
  -H "Content-Type: application/json" \
  -H "X-Tenant-Id: $TENANT" \
  -H "X-Temper-Principal-Kind: agent" \
  -H "X-Temper-Principal-Id: checkout-bot" \
  -d '{}' | python3 -c "
import sys,json
r = json.load(sys.stdin)
print(f\"  Status: {r['status']}\")
" 2>/dev/null
echo ""

# Step 9: Agent audit trail
echo "========================================="
echo "  AGENT AUDIT TRAIL"
echo "========================================="
curl -s "$API/observe/agents" | python3 -c "
import sys,json
data = json.load(sys.stdin)
for a in data['agents']:
    print(f\"Agent: {a['agent_id']}\")
    print(f\"  Actions: {a['total_actions']} total\")
    print(f\"  Success: {a['success_count']}\")
    print(f\"  Denied:  {a['denial_count']}\")
    print(f\"  Rate:    {a['success_rate']*100:.0f}%\")
    print(f\"  Entities: {', '.join(a['entity_types'])}\")
" 2>/dev/null
echo ""

echo "========================================="
echo "  ACTION HISTORY"
echo "========================================="
curl -s "$API/observe/agents/checkout-bot/history" | python3 -c "
import sys,json
data = json.load(sys.stdin)
print(f\"{'Action':<20} {'Result':<10} {'From':<15} {'To':<15}\")
print('-' * 60)
for h in data['history']:
    denied = 'DENIED' if h.get('authz_denied') else ('OK' if h['success'] else 'FAIL')
    frm = h.get('from_status') or '-'
    to = h.get('to_status') or '-'
    print(f\"{h['action']:<20} {denied:<10} {frm:<15} {to:<15}\")
" 2>/dev/null
echo ""

echo "========================================="
echo "  Open the dashboard: http://localhost:3000"
echo "  - Decisions page: see approval history"
echo "  - Agents page: see checkout-bot stats"
echo "========================================="
