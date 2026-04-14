# Crucible — Reference agent-runtime control plane on Temper

Crucible is a Temper reference app that re-implements a subset of Anthropic's
[Claude Managed Agents API](https://platform.claude.com/docs/en/managed-agents/overview)
on top of Temper. It exists to demonstrate two things:

1. **How an agent-runtime control plane maps onto Temper's modelling primitives**
   (entities, state machines, Cedar policies, OData routes).
2. **How to declare cross-field validation in specs** using the
   `[[field_invariant]]` grammar introduced in ADR-0041.

> **Crucible is not wire-compatible with Anthropic's API.** It shares the
> conceptual shape (Environment, Agent, Session, Event) but speaks OData,
> flattens `config` into scalar CSDL properties, rejects certain combinations
> that the upstream API accepts, and omits `Byoc`. See
> [ADR-0042](../../docs/adrs/0042-crucible-reference-app.md) for the full
> list of deliberate divergences.

## What this PR ships

This is **Phase 0**: the Environment entity with full CRUD (Create, List,
Get, Update, Delete, Archive) plus two child entities
(`EnvironmentAllowedHost`, `EnvironmentPackage`). The full config surface
is supported — both `Local` and `Cloud` config types, both `Unrestricted`
and `Limited` networking, `AllowMcpServers`, `AllowPackageManagers`,
`allowed_hosts[]`, and `packages[]` — with a **hard constraint** that
rejects any `Local` environment that sets cloud-only fields.

Follow-ups (Phase 1–3) will add Agent, Session, and Event entities, each
with its own PR.

## Layout

```
reference-apps/crucible/
├── Cargo.toml                  crate definition (dev-dependencies only)
├── README.md                   this file
├── src/lib.rs                  doc stub
├── specs/
│   ├── environment.ioa.toml         Environment IOA + field invariants
│   ├── environment_allowed_host.ioa.toml
│   ├── environment_package.ioa.toml
│   ├── cross-invariants.toml        parent-field cross-invariants
│   ├── model.csdl.xml               CSDL for Temper.Crucible namespace
│   └── policies/                    Cedar stubs (permissive for Phase 0)
└── tests/
    ├── crucible_cascade.rs          L1–L3 verification cascade per entity
    ├── crucible_dst.rs              deterministic scripted scenarios
    └── crucible_validation.rs       end-to-end HTTP tests over the router
```

## Running the tests

From the repository root:

```bash
# 1. verification cascade (L1 Stateright model-check, L2 simulation, L3 property test)
cargo test -p crucible-reference --test crucible_cascade

# 2. scripted DST scenarios + determinism proof
cargo test -p crucible-reference --test crucible_dst

# 3. end-to-end HTTP validation — every constraint branch through the real OData pipeline
cargo test -p crucible-reference --test crucible_validation

# all three at once
cargo test -p crucible-reference
```

`crucible_validation` is the primary correctness gate for the hard `Local`
constraint — it is the only test that exercises "POST Local environment
with forbidden fields → 409" end-to-end through the real OData router.

## Deliberate divergences from Anthropic's upstream shape

| Area | Anthropic | Crucible |
| --- | --- | --- |
| `config` | nested object | flattened to scalar CSDL properties |
| `metadata` | `map[string]string` | `Edm.String` holding JSON |
| `packages` | six parallel arrays (`pip`, `npm`, `apt`, `cargo`, `gem`, `go`) | single `EnvironmentPackage` child entity with `Manager` enum discriminator |
| `allowed_hosts` | `string[]` inline | `EnvironmentAllowedHost` child entity |
| `Local` + `allow_mcp_servers` | semantically inert | **rejected with 409** |
| `Byoc` config type | supported | out of scope for Phase 0 |

See [ADR-0042](../../docs/adrs/0042-crucible-reference-app.md) for the
full rationale behind each divergence.

## Related ADRs

- [ADR-0041: IOA field invariants + cross-invariant parent-field lookups](../../docs/adrs/0041-ioa-field-invariants.md)
  — the platform grammar Crucible pioneers.
- [ADR-0042: Crucible reference app](../../docs/adrs/0042-crucible-reference-app.md)
  — this app's design decisions and roadmap.
