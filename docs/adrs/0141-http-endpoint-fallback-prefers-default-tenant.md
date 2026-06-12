# ADR-0141: Header-less HttpEndpoint Requests Resolve to the Default Tenant

## Status

Accepted

## Context

The HttpEndpoint surface terminates foreign wire protocols — git smart-HTTP
and the GitHub REST shim — whose clients cannot send `X-Tenant-Id`. For those
header-less requests the router fell back to **the first registered
non-system tenant** in registry iteration order.

That made tenant resolution an accident of what else is registered on the
server. On the production Genesis deployment, Directed Evolution control
tenants (`de-control-…`) sort before `default`, so every header-less git and
REST request resolved to a DE tenant whose HttpEndpoint table is empty and
was answered `404 no route matches` — while the same request with an explicit
`X-Tenant-Id: default` header dispatched correctly. Local and CI runs never
saw this because a fresh server registers only `default`.

## Decision

`http_endpoint_fallback_tenant` resolves a header-less HttpEndpoint request
to, in order:

1. the registered `default` tenant (`TenantId::default()`) — protocol
   endpoint rows live there on single-operator deployments;
2. otherwise the first non-system tenant;
3. otherwise any registered tenant.

Requests that carry `X-Tenant-Id` are unchanged. Multi-tenant deployments
that need strict protocol-tenant routing should encode the tenant in the
HttpEndpoint row's path prefix, as before.

## Consequences

- Header-less protocol resolution is deterministic and independent of which
  other tenants exist on the server; the production 404s disappear.
- A deployment that deliberately serves protocol endpoints from a non-default
  tenant *and* has no `default` tenant registered keeps today's behavior
  (first non-system tenant).
- Covered by unit tests (`http_endpoint_fallback_tenant_*` in
  `router_test.rs`); verified live by the Genesis production workflow smoke.
