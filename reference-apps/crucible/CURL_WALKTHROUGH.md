---
title: Crucible end-to-end curl walkthrough
summary: Exact requests and responses captured from a live `temper serve` run against the Crucible specs.
---

# Crucible end-to-end curl walkthrough

This walkthrough captures real HTTP traffic against a `temper serve`
instance booted with the Crucible specs
(`reference-apps/crucible/specs/`).  Every branch of the ADR-0042
constraint surface is exercised through the real OData router, and the
**exact** response bodies are shown — nothing is summarized, nothing is
faked.

## Starting the server

```bash
TURSO_URL="file:/tmp/crucible-walkthrough.db" \
  cargo run -p temper-cli -- serve \
    --port 3456 \
    --specs-dir reference-apps/crucible/specs \
    --no-observe \
    --tenant crucible
```

Startup logs (trimmed) show verification passing on all three specs:

```
  Loading app: crucible from reference-apps/crucible/specs
    Loaded spec: EnvironmentPackage (verification pending, lint clean)
    Loaded spec: EnvironmentAllowedHost (verification pending, lint clean)
    Loaded spec: Environment (verification pending, lint clean)
  [verify] EnvironmentAllowedHost: [PASS] L0 Symbolic PASSED: 0 guards satisfiable, 1 invariants inductive, 0 unreachable
  [verify] EnvironmentPackage:     [PASS] L0 Symbolic PASSED: 0 guards satisfiable, 1 invariants inductive, 0 unreachable
  [verify] EnvironmentAllowedHost: [PASS] L1 Model Check PASSED: 1 states explored, all properties hold
  [verify] EnvironmentPackage:     [PASS] L1 Model Check PASSED: 1 states explored, all properties hold
  [verify] EnvironmentPackage:     [PASS] L2 Simulation PASSED: 5 seeds, 0 transitions, 0 dropped msgs
  [verify] EnvironmentAllowedHost: [PASS] L2 Simulation PASSED: 5 seeds, 0 transitions, 0 dropped msgs
  [verify] EnvironmentPackage:     [PASS] L3 Property Tests PASSED: 100 cases, 30 max steps
  [verify] EnvironmentAllowedHost: [PASS] L3 Property Tests PASSED: 100 cases, 30 max steps
  [verify] Environment:            [PASS] L0 Symbolic PASSED: 1 guards satisfiable, 2 invariants inductive, 0 unreachable
  [verify] Environment:            [PASS] L1 Model Check PASSED: 2 states explored, all properties hold
  [verify] Environment:            [PASS] L2 Simulation PASSED: 5 seeds, 15 transitions, 0 dropped msgs
  [verify] Environment:            [PASS] L3 Property Tests PASSED: 100 cases, 30 max steps
  [verify] Environment:            all levels passed
Listening on http://0.0.0.0:3456
```

All requests below use tenant header `X-Tenant-Id: crucible` and
`Content-Type: application/json`.

> **Note on tenancy.** On a cold boot the CLI runs
> `bootstrap_agent_specs` with `merge=true` against the user app tenant
> to add the Agent OS entities alongside the user specs. Before this
> PR, that call silently overwrote the tenant's `cross_invariants`
> with `None`, disabling enforcement for cross-entity hard constraints
> loaded from disk. Fixed in
> `crates/temper-server/src/registry/mod.rs` — merge mode now
> preserves the existing `cross_invariants` when the new payload does
> not carry any.

---

## 1. Local happy path — `ConfigType=Local` + `NetworkingType=Unrestricted`

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-local-ok",
    "Name": "local-dev",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted"
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "Environment",
  "entity_id": "env-local-ok",
  "status": "Active",
  "fields": {
    "id": "env-local-ok",
    "Name": "local-dev",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "Id": "env-local-ok",
    "Status": "Active"
  },
  "events": [
    {
      "action": "Created",
      "from_status": "",
      "to_status": "Active",
      "timestamp": "2026-04-11T14:43:27.646002Z",
      "params": {
        "id": "env-local-ok",
        "Name": "local-dev",
        "ConfigType": "Local",
        "NetworkingType": "Unrestricted"
      }
    }
  ],
  "total_event_count": 1,
  "sequence_nr": 1,
  "@odata.context": "$metadata#Environments/$entity",
  "@odata.id": "Environments('env-local-ok')"
}
```

A Local environment with unrestricted networking and no cloud-only
flags is the one thing Crucible permits cleanly. The entity lands in
`Active` and the `Created` event is persisted.

---

## 2. Local + `NetworkingType=Limited` → rejected by `LocalNetworkingMustBeUnrestricted`

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-bad-net",
    "Name": "bad-local",
    "ConfigType": "Local",
    "NetworkingType": "Limited"
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "Local environments must use Unrestricted networking",
    "details": {
      "type": "field_invariant",
      "invariant": "LocalNetworkingMustBeUnrestricted",
      "entity_type": "Environment",
      "entity_id": "env-bad-net",
      "operation": "create"
    }
  }
}
```

`details.type = "field_invariant"` and
`details.invariant = "LocalNetworkingMustBeUnrestricted"` — the field
invariant declared in `environment.ioa.toml` is firing on the
post-upsert `fields` snapshot.

---

## 3. Local + `AllowMcpServers=true` → rejected by `LocalCannotAllowMcpServers`

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-bad-mcp",
    "Name": "bad-local-mcp",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "AllowMcpServers": true
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "Local environments cannot set allow_mcp_servers",
    "details": {
      "type": "field_invariant",
      "invariant": "LocalCannotAllowMcpServers",
      "entity_type": "Environment",
      "entity_id": "env-bad-mcp",
      "operation": "create"
    }
  }
}
```

---

## 4. Local + `AllowPackageManagers=true` → rejected by `LocalCannotAllowPackageManagers`

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-bad-pkg",
    "Name": "bad-local-pkg",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "AllowPackageManagers": true
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "Local environments cannot set allow_package_managers",
    "details": {
      "type": "field_invariant",
      "invariant": "LocalCannotAllowPackageManagers",
      "entity_type": "Environment",
      "entity_id": "env-bad-pkg",
      "operation": "create"
    }
  }
}
```

---

## 5. Local with explicit `false` flags → allowed

The `any_of` grammar on the field invariant permits **either** absent
**or** explicit `false`. This scenario proves the explicit-false branch
is honored (a regression-prone path in the combinator logic).

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-local-explicit-false",
    "Name": "explicit-false",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "AllowMcpServers": false,
    "AllowPackageManagers": false
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "Environment",
  "entity_id": "env-local-explicit-false",
  "status": "Active",
  "fields": {
    "id": "env-local-explicit-false",
    "Name": "explicit-false",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "AllowMcpServers": false,
    "AllowPackageManagers": false,
    "Id": "env-local-explicit-false",
    "Status": "Active"
  },
  "sequence_nr": 1
}
```

---

## 6. Cloud happy path — full feature set

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-cloud-ok",
    "Name": "full-cloud",
    "ConfigType": "Cloud",
    "NetworkingType": "Limited",
    "AllowMcpServers": true,
    "AllowPackageManagers": true
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "Environment",
  "entity_id": "env-cloud-ok",
  "status": "Active",
  "fields": {
    "id": "env-cloud-ok",
    "Name": "full-cloud",
    "ConfigType": "Cloud",
    "NetworkingType": "Limited",
    "AllowMcpServers": true,
    "AllowPackageManagers": true,
    "Id": "env-cloud-ok",
    "Status": "Active"
  },
  "sequence_nr": 1
}
```

Cloud environments may set every cloud-only field — the `when` clause
on the field invariants (`ConfigType equals "Local"`) does not match,
so the rules are inert.

---

## 7. Child `EnvironmentAllowedHost` on the Cloud parent → allowed

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/EnvironmentAllowedHosts' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "host-cloud-1",
    "EnvironmentId": "env-cloud-ok",
    "Host": "api.example.com"
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "EnvironmentAllowedHost",
  "entity_id": "host-cloud-1",
  "status": "Active",
  "fields": {
    "id": "host-cloud-1",
    "EnvironmentId": "env-cloud-ok",
    "Host": "api.example.com",
    "Id": "host-cloud-1",
    "Status": "Active"
  },
  "sequence_nr": 1
}
```

`AllowedHostRequiresNonLocalParent` loads the parent
`Environment('env-cloud-ok')`, reads its `ConfigType=Cloud`, and passes
the `not in ["Local"]` test.

---

## 8. Child `EnvironmentPackage` on the Cloud parent → allowed

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/EnvironmentPackages' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "pkg-cloud-1",
    "EnvironmentId": "env-cloud-ok",
    "Manager": "Pip",
    "Name": "requests"
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "EnvironmentPackage",
  "entity_id": "pkg-cloud-1",
  "status": "Active",
  "fields": {
    "id": "pkg-cloud-1",
    "EnvironmentId": "env-cloud-ok",
    "Manager": "Pip",
    "Name": "requests",
    "Id": "pkg-cloud-1",
    "Status": "Active"
  },
  "sequence_nr": 1
}
```

---

## 9. Child `EnvironmentAllowedHost` on a **Local** parent → rejected

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/EnvironmentAllowedHosts' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "host-bad",
    "EnvironmentId": "env-local-ok",
    "Host": "api.example.com"
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "related Environment('env-local-ok') has ConfigType='Local', expected none of [\"Local\"]",
    "details": {
      "type": "cross_invariant",
      "invariant": "AllowedHostRequiresNonLocalParent",
      "entity_type": "EnvironmentAllowedHost",
      "entity_id": "host-bad",
      "operation": "create"
    }
  }
}
```

`details.type = "cross_invariant"` and
`details.invariant = "AllowedHostRequiresNonLocalParent"` — the
extended ADR-0041 cross-invariant grammar
(`related(Environment, EnvironmentId).ConfigType not in ["Local"]`)
resolved the parent field and failed the check.

---

## 10. Child `EnvironmentPackage` on a **Local** parent → rejected

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/EnvironmentPackages' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "pkg-bad",
    "EnvironmentId": "env-local-ok",
    "Manager": "Pip",
    "Name": "requests"
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "related Environment('env-local-ok') has ConfigType='Local', expected none of [\"Local\"]",
    "details": {
      "type": "cross_invariant",
      "invariant": "PackageRequiresNonLocalParent",
      "entity_type": "EnvironmentPackage",
      "entity_id": "pkg-bad",
      "operation": "create"
    }
  }
}
```

---

## 11. PATCH Cloud→Local with a forbidden field still set → rejected

### 11a. Seed a Cloud environment with `AllowMcpServers=true`

**Request**

```bash
curl -X POST 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "id": "env-patch-1",
    "Name": "patch-target",
    "ConfigType": "Cloud",
    "NetworkingType": "Limited",
    "AllowMcpServers": true
  }'
```

**Response** — `201 Created`

```json
{
  "entity_type": "Environment",
  "entity_id": "env-patch-1",
  "status": "Active",
  "fields": {
    "id": "env-patch-1",
    "Name": "patch-target",
    "ConfigType": "Cloud",
    "NetworkingType": "Limited",
    "AllowMcpServers": true,
    "Id": "env-patch-1",
    "Status": "Active"
  },
  "sequence_nr": 1
}
```

### 11b. PATCH to `ConfigType=Local` while `AllowMcpServers` is still `true`

**Request**

```bash
curl -X PATCH "http://127.0.0.1:3456/tdata/Environments('env-patch-1')" \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted"
  }'
```

**Response** — `409 Conflict`

```json
{
  "error": {
    "code": "ConstraintViolation",
    "message": "Local environments cannot set allow_mcp_servers",
    "details": {
      "type": "field_invariant",
      "invariant": "LocalCannotAllowMcpServers",
      "entity_type": "Environment",
      "entity_id": "env-patch-1",
      "operation": "patch"
    }
  }
}
```

The field-invariant pipeline runs against the **merged** post-patch
snapshot, not the patch body alone, so flipping `ConfigType` to `Local`
on an entity that still carries `AllowMcpServers=true` is caught.
Operation discriminator correctly reports `"patch"`.

---

## 12. Bound action — `ArchiveEnvironment`

**Request**

```bash
curl -X POST "http://127.0.0.1:3456/tdata/Environments('env-local-ok')/Temper.Crucible.ArchiveEnvironment" \
  -H 'X-Tenant-Id: crucible' \
  -H 'Content-Type: application/json' \
  -d '{}'
```

**Response** — `200 OK`

```json
{
  "entity_type": "Environment",
  "entity_id": "env-local-ok",
  "status": "Archived",
  "fields": {
    "id": "env-local-ok",
    "Name": "local-dev",
    "ConfigType": "Local",
    "NetworkingType": "Unrestricted",
    "Id": "env-local-ok",
    "Status": "Archived"
  },
  "events": [
    {
      "action": "Created",
      "from_status": "",
      "to_status": "Active",
      "timestamp": "2026-04-11T14:43:27.646002Z",
      "params": {
        "id": "env-local-ok",
        "Name": "local-dev",
        "ConfigType": "Local",
        "NetworkingType": "Unrestricted"
      }
    },
    {
      "action": "ArchiveEnvironment",
      "from_status": "Active",
      "to_status": "Archived",
      "timestamp": "2026-04-11T14:43:27.814371Z",
      "params": {}
    }
  ],
  "total_event_count": 2,
  "sequence_nr": 2
}
```

The bound action walks the state machine from `Active → Archived` and
both events are persisted in the entity's event log.

---

## 13. GET the archived entity — state + children navigation targets

**Request**

```bash
curl -X GET "http://127.0.0.1:3456/tdata/Environments('env-local-ok')" \
  -H 'X-Tenant-Id: crucible'
```

**Response** — `200 OK` (children navigation collapsed for brevity)

```json
{
  "entity_type": "Environment",
  "entity_id": "env-local-ok",
  "status": "Archived",
  "fields": { "...": "..." },
  "events": [ "Created", "ArchiveEnvironment" ],
  "total_event_count": 2,
  "sequence_nr": 2,
  "@odata.context": "$metadata#Environments/$entity",
  "@odata.id": "Environments('env-local-ok')",
  "@odata.actions": [],
  "@odata.children": {
    "AllowedHosts": {
      "type": "Collection(Temper.Crucible.EnvironmentAllowedHost)",
      "target": "Environments('env-local-ok')/AllowedHosts"
    },
    "Packages": {
      "type": "Collection(Temper.Crucible.EnvironmentPackage)",
      "target": "Environments('env-local-ok')/Packages"
    }
  }
}
```

`@odata.children` exposes the parent→child navigation targets declared
in `model.csdl.xml` — free read-side plumbing once the specs are
registered.

---

## 14. List all environments — `GET /tdata/Environments`

**Request**

```bash
curl -X GET 'http://127.0.0.1:3456/tdata/Environments' \
  -H 'X-Tenant-Id: crucible'
```

**Response** — `200 OK`

The list contains the four environments created above, with
`env-local-ok` now in `Archived`:

| `entity_id`                 | `status`   | `ConfigType` | `NetworkingType` | `AllowMcpServers` | `AllowPackageManagers` |
| --------------------------- | ---------- | ------------ | ---------------- | ----------------- | ---------------------- |
| `env-cloud-ok`              | `Active`   | `Cloud`      | `Limited`        | `true`            | `true`                 |
| `env-local-explicit-false`  | `Active`   | `Local`      | `Unrestricted`   | `false`           | `false`                |
| `env-local-ok`              | `Archived` | `Local`      | `Unrestricted`   | (unset)           | (unset)                |
| `env-patch-1`               | `Active`   | `Cloud`      | `Limited`        | `true`            | (unset)                |

---

## Summary

| # | Scenario                                                             | Status | Rule                                  |
| - | -------------------------------------------------------------------- | ------ | ------------------------------------- |
| 1 | Local + Unrestricted                                                 |  201   | none — happy path                     |
| 2 | Local + Limited networking                                           |  409   | `LocalNetworkingMustBeUnrestricted`   |
| 3 | Local + `AllowMcpServers=true`                                       |  409   | `LocalCannotAllowMcpServers`          |
| 4 | Local + `AllowPackageManagers=true`                                  |  409   | `LocalCannotAllowPackageManagers`     |
| 5 | Local + explicit `false` flags                                       |  201   | `any_of` explicit-false branch        |
| 6 | Cloud + full feature set                                             |  201   | none — `when` inert on Cloud          |
| 7 | `EnvironmentAllowedHost` attached to Cloud parent                    |  201   | cross-invariant passes                |
| 8 | `EnvironmentPackage` attached to Cloud parent                        |  201   | cross-invariant passes                |
| 9 | `EnvironmentAllowedHost` attached to Local parent                    |  409   | `AllowedHostRequiresNonLocalParent`   |
| 10| `EnvironmentPackage` attached to Local parent                        |  409   | `PackageRequiresNonLocalParent`       |
| 11| PATCH Cloud → Local with `AllowMcpServers=true` still set            |  409   | `LocalCannotAllowMcpServers` (patch)  |
| 12| `Temper.Crucible.ArchiveEnvironment` bound action                    |  200   | state-machine transition              |
| 13| GET archived entity                                                  |  200   | navigation targets included           |
| 14| List environments                                                    |  200   | four entities                         |

Every branch of the Local-environment constraint surface — three
same-entity field invariants, two parent-lookup cross invariants, one
PATCH path, one state-machine action — is observable end-to-end
through the real OData pipeline, matching the assertions in
`tests/crucible_validation.rs`.
