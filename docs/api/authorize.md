# Authorization questions

`POST /api/authorize` asks Cedar about offers without making a move. A question
writes nothing: no PendingDecision, GovernanceDecision, denial-pattern or
trajectory row, and nothing for the suggestion engine. A real request an agent
sends and is refused (a read by id, a list it may not make, a dispatch) still
files one decision its person can approve. While that decision waits, the same
agent refused the same action on the same resource gets it back instead of a
new one.
Authentication and tenant resolution use the same credential-derived context as
before. `agent_id` must match that principal (including person principals).

For one record or a page, send a single request:

```json
{
  "agent_id": "agent-1",
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "context": {"owner": "person-1", "status": "Open", "has_spec": true},
      "actions": ["Read", "Approve"]
    }
  ]
}
```

The HTTP 200 answer preserves record order and action order, including duplicates:

```json
{
  "records": [
    {
      "resource_type": "Release",
      "resource_id": "release-1",
      "actions": [
        {"action": "Read", "allowed": true, "reason": null},
        {"action": "Approve", "allowed": false, "reason": "Authorization denied ..."}
      ]
    }
  ]
}
```

`reason` is the existing Cedar denial text; an allow has a null reason. No check
answer contains `decision_id`. Empty record/action lists return empty answers.
A malformed context (anything except an object or null) rejects the entire page
with HTTP 400 before evaluation. A mismatched principal returns HTTP 403.
Mixed single/batch bodies are rejected rather than silently dropping questions.

The caller supplies each record snapshot once in `context`, with exactly the
attributes it previously supplied to each single check. Omitted/null context is
an empty object. The kernel overwrites `context.id` with `resource_id`, as before.
This endpoint does not retrieve or grant access to records; callers must read
records through the governed read surface first. Offers describe that read's
snapshot. Dispatch always reloads and authorizes the actual move independently.

For R records with M total requested moves, the cost is M Cedar evaluations,
R decoded context maps, **zero store reads** and **zero store writes**.
No per-move record/context reload is performed. A denied check is visible only
as the authz wide event every Cedar check emits. Conformance verdicts retain
their existing meaning.
There is no arbitrary item cap: the existing Axum JSON request-body byte limit
(default 2 MiB on this route) bounds input. Pages over that body limit must be
split by bytes, not by an invented move count.

The published Rust and TypeScript SDKs also retain their single-pair form:
`{agent_id, action, resource_type, resource_id, context?}` returns
`{allowed, reason}` without a decision ID. Both SDKs expose a batch method.
MCP does not call this check route; its decision IDs come from refused mutations
and remain available for human resolution.

Katagami migration: replace the per-move calls in `authorizedRecord` with one
record entry containing all offered kernel action names. `authorizedRecords`
can put every page record into the same request. Preserve the current resource
attributes; zip the ordered action answers back onto the offers. Do not treat a
failed HTTP call as permission. Follow with staging verification on a test
person's app: one Factory-sized read makes one authorize request, matches its
prior offers, adds no pending decision, and both the site `mcp read_app` span and
the person Temper's authorize span meet Rita's 1 s bar in Datadog. That staging
measurement requires the subsequent site release; local timings do not prove it.
