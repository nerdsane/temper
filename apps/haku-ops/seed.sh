#!/bin/bash
# Seed haku-ops with real historical proposal data
# Run after each server restart until event replay is wired

BASE="http://localhost:3001/tdata"
CT="Content-Type: application/json"

seed() {
  local title="$1" num="$2" target="$3"
  
  RESP=$(curl -s -X POST "$BASE/Proposals" -H "$CT" \
    -d "{\"Title\": \"$title\", \"ProposalNumber\": \"$num\"}")
  ID=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin)['entity_id'])")
  
  [ "$target" = "Scratched" ] && {
    curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.Scratch" -H "$CT" -d '{"Reason": "Rita said no"}'
    echo "  $num → Scratched"; return; }
  [ "$target" = "Seed" ] && { echo "  $num → Seed"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.WritePlan" -H "$CT" \
    -d "{\"ArchDoc\": \"proposals/$num.md\", \"RiskTier\": \"low\"}"
  [ "$target" = "Planned" ] && { echo "  $num → Planned"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.Approve" -H "$CT" -d '{"ApprovedBy": "rita"}'
  [ "$target" = "Approved" ] && { echo "  $num → Approved"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.StartImplementation" -H "$CT" \
    -d "{\"CCSessionId\": \"cc-$num\"}"
  [ "$target" = "Implementing" ] && { echo "  $num → Implementing"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.AttachShowboat" -H "$CT" \
    -d "{\"ShowboatDoc\": \"reports/$num-complete.md\"}"
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.CompleteImplementation" -H "$CT" \
    -d '{"Summary": "Done"}'
  [ "$target" = "Completed" ] && { echo "  $num → Completed"; return; }
  
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.MarkCIPassed" -H "$CT" \
    -d "{\"RunId\": \"ci-$num\"}"
  curl -s -o /dev/null -X POST "$BASE/Proposals('$ID')/HakuOps.VerifyDeployment" -H "$CT" \
    -d '{"DeploymentUrl": "https://deep-sci-fi.world", "HealthCheckResult": "200 OK"}'
  echo "  $num → Verified"
}

echo "🌱 Seeding proposals..."
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
echo "🔍 Seeding findings..."
for item in \
  '{"Description":"DST regression: test_post_heartbeat_world_signals failing","Category":"test","Priority":"high"}' \
  '{"Description":"stop-verify-deploy.sh worlds key bug","Category":"ci","Priority":"high"}' \
  '{"Description":"Feedback Triage workflow CI failing every run","Category":"ci","Priority":"medium"}' \
  '{"Description":"Video regen: 40/65 stories unverified","Category":"content","Priority":"medium"}' \
  '{"Description":"7 open backlog items from Feb 15","Category":"backlog","Priority":"low"}'; do
  curl -s -X POST "$BASE/Findings" -H "$CT" -d "$item" | \
    python3 -c "import sys,json; d=json.load(sys.stdin); print(f'  {d[\"entity_id\"][:8]}... → Observed')"
done

echo ""
P=$(curl -s "$BASE/Proposals" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('value',[])))")
F=$(curl -s "$BASE/Findings" | python3 -c "import sys,json; print(len(json.load(sys.stdin).get('value',[])))")
echo "✅ Seeded: $P proposals, $F findings"
