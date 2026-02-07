#!/bin/bash
# Temper End-to-End Demo
# Demonstrates: server, agent, persistence, trajectory capture, analysis
#
# Prerequisites:
#   docker compose up -d
#   ANTHROPIC_API_KEY set in environment
#
# Usage: ./scripts/demo.sh

set -e

export DATABASE_URL=postgres://temper:temper_dev@localhost:5432/temper
export CLICKHOUSE_URL=http://localhost:8123
export RUST_LOG=error

echo "============================================"
echo "  TEMPER END-TO-END DEMO"
echo "============================================"
echo ""

# 1. Ensure infrastructure is running
echo "Step 1: Checking infrastructure..."
docker compose ps --format "table {{.Name}}\t{{.Status}}" 2>/dev/null | head -5
echo ""

# 2. Clean previous data
echo "Step 2: Cleaning previous data..."
docker exec temper-postgres-1 psql -U temper -d temper -c "DELETE FROM events; DELETE FROM snapshots;" 2>/dev/null
curl -s -X POST "$CLICKHOUSE_URL/" -d "TRUNCATE TABLE spans" 2>/dev/null
echo "  Done."
echo ""

# 3. Start server
echo "Step 3: Starting server with Postgres persistence..."
cargo run -p ecommerce &>/tmp/temper-demo-server.log &
SERVER_PID=$!
sleep 3
echo "  Server running (PID $SERVER_PID)"
echo ""

# 4. Run agent scenarios
echo "Step 4: Running agent scenarios..."
echo ""

echo "  Scenario 1: Create and submit an order"
cargo run -p ecommerce -- agent "Create a new order, add a premium widget, and submit it" 2>/dev/null
echo ""

echo "  Scenario 2: Try to cancel a shipped order (should fail gracefully)"
cargo run -p ecommerce -- agent "Cancel my order that was already shipped" 2>/dev/null
echo ""

echo "  Scenario 3: Unmet intent (split order)"
cargo run -p ecommerce -- agent "I want to split my order into two shipments" 2>/dev/null
echo ""

# 5. Show what's in Postgres
echo "============================================"
echo "Step 5: Postgres Event Store"
echo "============================================"
docker exec temper-postgres-1 psql -U temper -d temper -c \
  "SELECT entity_id, event_type, created_at FROM events ORDER BY id;" 2>/dev/null
echo ""

# 6. Show what's in ClickHouse
echo "============================================"
echo "Step 6: ClickHouse Trajectory Spans"
echo "============================================"
curl -s "$CLICKHOUSE_URL/?query=SELECT+trace_id,operation,status,JSONExtractString(attributes,'user_intent')+as+intent+FROM+spans+ORDER+BY+start_time+FORMAT+PrettyCompact" 2>/dev/null
echo ""

# 7. Run trajectory analysis
echo "============================================"
echo "Step 7: Trajectory Analysis + Evolution Records"
echo "============================================"
cargo run -p ecommerce -- analyze 2>/dev/null
echo ""

# 8. Stop server
kill $SERVER_PID 2>/dev/null
wait $SERVER_PID 2>/dev/null

echo "============================================"
echo "  DEMO COMPLETE"
echo "============================================"
echo ""
echo "What happened:"
echo "  - Claude interpreted 3 natural language requests"
echo "  - Entity actors processed state machine transitions"
echo "  - Events persisted to PostgreSQL"
echo "  - Trajectory spans captured to ClickHouse"
echo "  - Analysis generated O-Records and I-Records"
echo "  - Product intelligence digest produced"
