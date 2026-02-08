#!/usr/bin/env bash
set -euo pipefail

# Unset Claude Code's OTEL env vars so our local config takes effect
unset OTEL_EXPORTER_OTLP_ENDPOINT
unset OTEL_EXPORTER_OTLP_TRACES_ENDPOINT
unset OTEL_EXPORTER_OTLP_METRICS_ENDPOINT
unset OTEL_EXPORTER_OTLP_LOGS_ENDPOINT
unset OTEL_EXPORTER_OTLP_PROTOCOL
unset OTEL_TRACES_EXPORTER
unset OTEL_METRICS_EXPORTER
unset OTEL_LOGS_EXPORTER
unset OTEL_METRICS_INCLUDE_VERSION
unset OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE

export RUST_LOG="info,opentelemetry=debug,opentelemetry_sdk=debug,opentelemetry_otlp=debug,opentelemetry_http=debug,reqwest=debug"
export DATABASE_URL="postgres://temper:temper_dev@localhost:5432/temper"
export CLICKHOUSE_URL="http://localhost:8123"
export OTLP_ENDPOINT="http://localhost:4318"

echo "=== Starting server ==="
cargo run -p ecommerce > /tmp/temper-e2e.log 2>&1 &
SERVER_PID=$!
echo "Server PID: $SERVER_PID"
sleep 4

echo "=== Creating order ==="
ORDER=$(curl -s -X POST http://localhost:3000/odata/Orders \
  -H "Content-Type: application/json" -d '{}')
ORDER_ID=$(echo "$ORDER" | python3 -c "import sys,json; print(json.load(sys.stdin).get('entity_id','?'))")
echo "Order: $ORDER_ID"

echo "=== Adding item ==="
curl -s -X POST "http://localhost:3000/odata/Orders('${ORDER_ID}')/Temper.Ecommerce.AddItem" \
  -H "Content-Type: application/json" \
  -H "X-Temper-Principal-Id: test" \
  -H "X-Temper-Principal-Kind: agent" \
  -d '{"ProductId":"laptop"}'
echo ""

echo "=== Submitting order ==="
curl -s -X POST "http://localhost:3000/odata/Orders('${ORDER_ID}')/Temper.Ecommerce.SubmitOrder" \
  -H "Content-Type: application/json" \
  -H "X-Temper-Principal-Id: test" \
  -H "X-Temper-Principal-Kind: agent" \
  -d '{}'
echo ""

echo "=== Waiting 8s for batch flush ==="
sleep 8

echo "=== Export-related logs ==="
grep -iE "export|CallingExport|ReqwestBlocking|Send|connection|error|localhost:4318" /tmp/temper-e2e.log | tail -30

echo ""
echo "=== ClickHouse otel_traces ==="
curl -s "http://localhost:8123/?query=SELECT+count(*)+as+cnt+FROM+otel_traces+FORMAT+JSONEachRow"

echo ""
echo "=== Collector logs ==="
docker compose logs otel-collector --since 30s 2>&1 | tail -20

# Cleanup
kill $SERVER_PID 2>/dev/null
