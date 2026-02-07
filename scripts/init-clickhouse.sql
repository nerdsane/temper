-- Temper ClickHouse initialization
-- Canonical observability schema (provider-agnostic, SQL-queryable)

-- Spans: distributed traces across actors
CREATE TABLE IF NOT EXISTS spans (
    trace_id        String,
    span_id         String,
    parent_span_id  Nullable(String),
    service         String,
    operation       String,
    status          String,
    duration_ns     UInt64,
    start_time      DateTime64(9, 'UTC'),
    end_time        DateTime64(9, 'UTC'),
    attributes      String   -- JSON object
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(start_time)
ORDER BY (service, start_time, trace_id)
TTL toDateTime(start_time) + INTERVAL 30 DAY;

-- Logs: structured log entries
CREATE TABLE IF NOT EXISTS logs (
    timestamp       DateTime64(9, 'UTC'),
    level           LowCardinality(String),
    service         String,
    message         String,
    attributes      String   -- JSON object
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (service, timestamp)
TTL toDateTime(timestamp) + INTERVAL 30 DAY;

-- Metrics: time-series measurements
CREATE TABLE IF NOT EXISTS metrics (
    metric_name     LowCardinality(String),
    timestamp       DateTime64(9, 'UTC'),
    value           Float64,
    tags            String   -- JSON object
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (metric_name, timestamp)
TTL toDateTime(timestamp) + INTERVAL 90 DAY;

-- Trajectories: agent execution traces (materialized view of spans)
CREATE TABLE IF NOT EXISTS trajectories (
    trace_id        String,
    user_intent     Nullable(String),
    prompt_version  Nullable(String),
    agent_id        Nullable(String),
    outcome         Nullable(String),
    feedback_score  Nullable(Float64),
    total_tokens    Nullable(UInt64),
    total_api_calls Nullable(UInt32),
    turn_count      UInt32,
    start_time      DateTime64(9, 'UTC'),
    end_time        DateTime64(9, 'UTC'),
    duration_ns     UInt64
) ENGINE = MergeTree()
PARTITION BY toYYYYMM(start_time)
ORDER BY (start_time, trace_id)
TTL toDateTime(start_time) + INTERVAL 90 DAY;
