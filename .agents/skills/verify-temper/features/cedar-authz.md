# Cedar authorization and the decision flow

## Sub-features
Schema-less Cedar evaluation (`crates/temper-authz/src/engine/mod.rs`), the `/api` policy + decision plane, denial surfacing, and the human approval flow (`temper decide`).

## How to get to it (user POV)
Every governed dispatch is authorized. A denied action does not vanish - it becomes a pending decision a human can approve, after which the action succeeds.

## Driving it
Evaluation is schema-less (`Request::new(principal, action, resource, context, None)`): actions/resources are tenant-defined, so no Cedar schema is supplied. Uids come from the dispatch - `Action::"{name}"`, `{PrincipalType}::"{id}"` (Customer/Agent/Admin/System), `{resource_type}::"{id}"`. The verified-operator predicate is the one const `VERIFIED_OPERATOR_WHEN` (`engine/mod.rs:732`): `principal has agent_type && principal.agent_type == "operator" && principal has agentTypeVerified && principal.agentTypeVerified == true` - both are PRINCIPAL attributes (not `context`), and `agentTypeVerified` is set true only on credential-resolved paths, false on self-declared enrichment.

```bash
# 1. A denied OData dispatch returns 403 with the pending-decision id embedded
curl -sS -X POST "http://localhost:3600/tdata/<Set>('<id>')/Temper.<Action>" \
  -H "Authorization: Bearer $AGENT_TOKEN" -H "X-Tenant-Id: default" -H "Content-Type: application/json" -d '{}'
#   -> 403 {"error":{"code":"AuthorizationDenied","message":"... (decision: PD-…)"}}

# 2. List pending decisions, 3. approve, 4. re-invoke -> 200
curl -sS "http://localhost:3600/api/tenants/default/decisions?status=pending" -H "Authorization: Bearer $TEMPER_API_KEY"
curl -sS -X POST "http://localhost:3600/api/tenants/default/decisions/PD-…/approve" \
  -H "Authorization: Bearer $TEMPER_API_KEY" -H "Content-Type: application/json" -d '{"scope":{…},"decided_by":"human-terminal"}'
```
Drive the approve loop with the curl flow above (lowercase `?status=pending`). NOT with `temper decide` today: it polls `?status=Pending` (`decide/mod.rs:122`) while decisions store lowercase (`DecisionStatus` is `serde(rename_all="lowercase")`), so its list comes back empty and it never surfaces the pending decision - a real CLI bug (filed as ARN-442), not a driver error.

Policy plane lives under `/api/tenants/{tenant}/policies` (GET/PUT, `/rules`, `/list`, `/create`, `/entry/{id}` PATCH/DELETE, `/suggestions`) and decisions under `/api/tenants/{tenant}/decisions` (list, `/stream` SSE, `/{id}`, `/{id}/approve`, `/{id}/deny`). `POST /api/authorize` is a non-blocking pre-flight probe: for a well-formed authenticated request it returns 200 with `{allowed:true|false, decision_id?, reason?}` - it does NOT 403 on a deny (the deny rides in the body), unlike the OData dispatch path which 403s. (A malformed body or missing auth still errors normally.)

## What proves it
The denied dispatch returns 403 carrying a `PD-…` id; that id appears in the pending-decisions list; after approve a Cedar permit / `GovernanceDecision` is created and the re-invoked action returns 200. The `PendingDecision` and `GovernanceDecision` entities are the durable evidence. A denial that never surfaces as a pending decision is a product finding, not a driver error.

## Gotchas
- `POST /api/audit` only records a trajectory entry; there is no `/api/audit` GET reader - audit history is read through the trajectory/observe endpoints.
- Decision status is stored lowercase (`DecisionStatus` is `#[serde(rename_all = "lowercase")]` in `state/pending_decisions.rs`), so the filter value is `?status=pending` - the CLI's `?status=Pending` will not match; use lowercase.
- A silent 403 is itself a finding (stack rule): surface every denial to the human channel.
