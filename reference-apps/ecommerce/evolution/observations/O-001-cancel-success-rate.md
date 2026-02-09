# O-001: CancelOrder Success Rate Below Threshold

**Type:** O-Record (Observation)
**Timestamp:** 2025-01-15T14:30:00Z
**Source:** Sentinel query on trajectory spans

## Observation

CancelOrder action success rate is 73% over the past 7 days. Agents frequently
attempt CancelOrder from invalid states (Processing, Shipped, Delivered).

## Evidence

```sql
SELECT
    count(*) as total,
    countIf(status = 'success') as successes,
    round(successes / total * 100, 1) as success_rate
FROM otel_traces
WHERE span_name = 'CancelOrder'
  AND timestamp > now() - INTERVAL 7 DAY
```

Result: 847 total attempts, 618 successes, 73.0% success rate.

## Top Failure Reasons

| Current State | Attempts | % of Failures |
|---------------|----------|---------------|
| Processing    | 102      | 44.5%         |
| Shipped       | 89       | 38.9%         |
| Delivered     | 38       | 16.6%         |

## Related

- **P-Record:** [P-001](../problems/P-001-cancel-agent-guidance.md)
