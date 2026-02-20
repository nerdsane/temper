#!/bin/bash
# Seed haku-ops with real historical proposal data
# Run after each server restart until event replay is wired

BASE="http://localhost:3001/tdata"
CT="Content-Type: application/json"
TN="X-Tenant-Id: haku-ops"

seed() {
  local title="$1" num="$2" target="$3"
  
  RESP=$(curl -s -X POST "$BASE/Proposals" -H "$CT" -H "$TN" \
    -d "{\"Title\": \"$title\", \"ProposalNumber\": \"$num\"}")
  ID=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['entity_id'])")
  
  [ "$target" = "Scratched" ] && {
    curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.Scratch" -H "$CT" -H "$TN" -d '{"Reason": "Rita said no"}'
    echo "  $num → Scratched"; return; }
  [ "$target" = "Seed" ] && { echo "  $num → Seed"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.WritePlan" -H "$CT" -H "$TN" \
    -d "{\"ArchDoc\": \"proposals/$num.md\", \"RiskTier\": \"low\"}"
  [ "$target" = "Planned" ] && { echo "  $num → Planned"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.Approve" -H "$CT" -H "$TN" -d '{"ApprovedBy": "rita"}'
  [ "$target" = "Approved" ] && { echo "  $num → Approved"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.StartImplementation" -H "$CT" -H "$TN" \
    -d "{\"CCSessionId\": \"cc-$num\"}"
  [ "$target" = "Implementing" ] && { echo "  $num → Implementing"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.AttachShowboat" -H "$CT" -H "$TN" \
    -d "{\"ShowboatDoc\": \"reports/$num-complete.md\"}"
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.CompleteImplementation" -H "$CT" -H "$TN" \
    -d '{"Summary": "Done"}'
  [ "$target" = "Completed" ] && { echo "  $num → Completed"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.MarkCIPassed" -H "$CT" -H "$TN" \
    -d "{\"RunId\": \"ci-$num\"}"
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.VerifyDeployment" -H "$CT" -H "$TN" \
    -d '{"DeploymentUrl": "https://deep-sci-fi.world", "HealthCheckResult": "200 OK"}'
  echo "  $num → Verified"
}

echo "🌱 Seeding proposals (tenant: haku-ops)..."
seed "Feed Pagination DST Rule" "PROP-002" "Verified"
seed "Relationship Graphs" "PROP-009" "Verified"
seed "Story Arc Detection" "PROP-011" "Verified"
seed "Seed Dead Worlds" "PROP-012" "Scratched"
seed "World Semantic Map" "PROP-015" "Verified"
seed "Directional Relationships" "PROP-022" "Verified"
seed "Typography + Video Sanitization" "PROP-023" "Verified"
seed "Map Cluster Label Fix" "PROP-024" "Seed"
seed "Story Embedding Backfill" "PROP-027" "Seed"
seed "Denormalized Feed Events" "PROP-028" "Verified"

echo ""
echo "🔍 Seeding findings (tenant: haku-ops)..."
for item in \
  '{"Description":"DST regression: test_post_heartbeat_world_signals failing","Category":"test","Priority":"high"}' \
  '{"Description":"stop-verify-deploy.sh worlds key bug","Category":"ci","Priority":"high"}' \
  '{"Description":"Feedback Triage workflow CI failing every run","Category":"ci","Priority":"medium"}' \
  '{"Description":"Video regen: 40/65 stories unverified","Category":"content","Priority":"medium"}' \
  '{"Description":"7 open backlog items from Feb 15","Category":"backlog","Priority":"low"}'; do
  curl -s -X POST "$BASE/Findings" -H "$CT" -H "$TN" -d "$item" | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'  {d[\"entity_id\"][:8]}... → Observed')"
done

echo ""
P=$(curl -s "$BASE/Proposals" -H "$TN" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('value',[])))")
F=$(curl -s "$BASE/Findings" -H "$TN" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('value',[])))")
echo "✅ Seeded: $P proposals, $F findings (tenant: haku-ops)"
