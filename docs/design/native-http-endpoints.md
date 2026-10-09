# HTTP transports admitted by IOA actions

`HttpEndpoint` already owns path matching and streaming WASM dispatch. A route may
now declare `NativeConfig` to select a host-installed I/O transport after one or
more ordinary OData bound actions have admitted the request.

```json
{
  "transport": "example-transport",
  "actions": [{
    "name": "target",
    "entity_set": "Targets",
    "entity_id": "{target_id}",
    "action": "Example.Serve",
    "params": {"requested_parent": "{parent_id}"}
  }]
}
```

The JSON is stored as a string in the endpoint's spec-declared `NativeConfig`
field. The owning app supplies its HttpEndpoint IOA/CSDL and management Cedar
policy. Transport registration itself grants neither a route nor permission to
call it. Existing routes without NativeConfig continue to use WASM unchanged.

The dispatcher uses the incoming immutable authenticated request context to call
the existing OData action handler. That handler performs Cedar authorization,
verification gating, authenticated parameter binding, input validation and IOA
transition dispatch with the existing authorization-state concurrency check.
Template captures are decoded once and escaped as OData string keys. They cannot
select a tenant, entity set, action or transport. Any failed admission prevents
transport execution. The caller's idempotency header is not used for admission:
every new request must check the current policy/state, even when it retries an
upstream operation. The original header remains available to the transport.

Admission requires an existing target, including for actions allowed in the initial
state. A declared route is not a creation grant: an authorized request selecting a
missing or deleted ID returns `404 EntityNotFound` without executing the transport.
Create the entity explicitly before using such a route. A successful transport's
HTTP status (including 201) describes the upstream exchange, not creation of the
local admission target. WASM endpoint `AdmissionActions` use this same boundary.

Successful action responses are delivered under their declared names alongside
method, URI, sanitized headers, public prefix and request bytes. Request bodies
and transport responses are request-scoped; they do not become entity fields.
A native transport implements actual I/O and returns an HTTP Response. It must
not invent parallel ownership or lifecycle checks. As with existing native agent
adapters, transport code is trusted host code, not tenant-loaded code.

Admission actions run in order and are not a multi-entity transaction. Apps should
use observational/self-loop actions for routing guards. An already admitted
request may complete while a later request changes entity state. Endpoint-table
configuration reconciliation retains the existing HttpEndpoint behavior.

Native routes require at least one admission and cannot also declare the WASM
action bridge. Exchanges are bounded by the endpoint timeout; incoming bodies and
individual admission responses are capped at 8 MiB. This implementation does not
promise an atomic transaction spanning local admission and remote I/O.

Verification: native endpoint tests exercise real OData/IOA dispatch, caller
policy denial, a state change between repeated requests, multiple admission
failure, empty admission rejection, binary bodies, query strings, OData headers,
and caller credential stripping. Existing WASM route tests remain applicable.
