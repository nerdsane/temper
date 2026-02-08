#!/usr/bin/env bash
set -euo pipefail

# Unset Claude Code's OTEL env vars
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

export DATABASE_URL="postgres://temper:temper_dev@localhost:5432/temper"
export CLICKHOUSE_URL="http://localhost:8123"
export OTLP_ENDPOINT="http://localhost:4318"

echo "=== Starting server ==="
RUST_LOG=info cargo run -p ecommerce > /tmp/temper-server.log 2>&1 &
SERVER_PID=$!
sleep 4

echo "=== Baseline trace count ==="
BEFORE=$(curl -s "http://localhost:8123/?query=SELECT+count(*)+as+cnt+FROM+otel_traces+FORMAT+JSONEachRow" | python3 -c "import sys,json; print(json.load(sys.stdin)['cnt'])")
echo "Before: $BEFORE traces"

echo "=== Running agent ==="
RUST_LOG=info cargo run -p ecommerce -- agent "Create a new order, add a headset to it, then submit the order" 2>&1

# Wait for batch flush
echo ""
echo "=== Waiting 8s for batch flush ==="
sleep 8

echo "=== After trace count ==="
AFTER=$(curl -s "http://localhost:8123/?query=SELECT+count(*)+as+cnt+FROM+otel_traces+FORMAT+JSONEachRow" | python3 -c "import sys,json; print(json.load(sys.stdin)['cnt'])")
echo "After: $AFTER traces"
echo "New traces: $((AFTER - BEFORE))"

echo ""
echo "=== All spans ==="
curl -s "http://localhost:8123/?query=SELECT+SpanName,ServiceName,StatusCode+FROM+otel_traces+ORDER+BY+Timestamp+DESC+FORMAT+JSONEachRow"

kill $SERVER_PID 2>/dev/null
