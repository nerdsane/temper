# ADR-0172: Seed a Narrow Operator `manage_policies` Permit at Credential Bootstrap

- Status: Accepted
- Date: 2026-08-20
- Deciders: Temper core maintainers
- Related:
  - ADR-0014: Governance gap closure (`manage_policies` on `PolicySet`)
  - ADR-0032: Granular Cedar policy storage
  - ADR-0033: Platform-assigned agent identity
  - ADR-0144: Idempotent Cedar merges
  - ADR-0157: Credential-bound Class A authentication edge
  - ARN-255 / #407: kernel token verification; the code-embedded
    `TrustedIssuer` / `PrincipalGeneration` identity-entity gate
    (`crates/temper-authz/src/engine/mod.rs`, `VERIFIED_OPERATOR_WHEN`)
  - `crates/temper-platform/src/bootstrap.rs` (`bootstrap_operator_credential`)
  - `crates/temper-server/src/api/decisions.rs` (approve / deny)
  - `crates/temper-server/src/api/decisions_access.rs`
  - `crates/temper-server/src/authz/policy_persistence.rs`
  - `crates/temper-server/tests/policy_authorization.rs`

## Context

A new tenant can boot and run installed apps. `temper serve --app` and
`install_os_app` are process load, not Cedar. After boot, Cedar is
default-deny.

Approving a denied action (`POST /api/tenants/{tenant}/decisions/{id}/approve`,
`temper decide`) and adding Cedar (the policy API) both require action
`manage_policies` on resource `PolicySet`. OS-app Cedar does not grant that.

`bootstrap_operator_credential` creates the operator `AgentType` and
`AgentCredential` so `TEMPER_API_KEY` resolves as a verified identity. It does
not persist or activate a `manage_policies` permit. On a virgin store the
operator therefore cannot approve a denial or add Cedar. That is the
governance loop, not "apps cannot install" and not "named actions the app
already permits."

This is ARN-389.

Closing that door is not enough. An agent who was denied must not be able
to approve or deny that same decision, even if they somehow have
`manage_policies`. "They lack the permit" is not the control.

## Decision

Two halves, both required:

1. When the operator credential is bootstrapped for a tenant, persist and
   activate a **narrow** Cedar permit through the same door as every other
   policy: validate, merge into the tenant's live Cedar, write a granular
   `policies` row, survive restart. Not a code bypass. Not permit-all.
2. On the approve and deny HTTP paths, reject the request when the
   caller's principal id equals `PendingDecision.agent_id`. 403. This is
   independent of Cedar.

### Sub-Decision 1: Seed inside `bootstrap_operator_credential`

The permit is created in `bootstrap_operator_credential` for **that**
tenant. Every caller that bootstraps an operator credential gets the door,
not only `"default"`. CLI Phase 8 today still registers the deployment key
in `default` only (ADR-0157); this ADR does not change that scope.

**Why this approach**: the missing permit is a property of the operator
identity, not of a particular tenant name. Wiring at the credential
function keeps the seed and the identity on the same path.

### Sub-Decision 2: Exact permit shape

Use the statement already proven in `policy_authorization.rs`:

```
permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource == PolicySet::"{tenant}"
) when {
  principal.agent_type == "operator" &&
  principal.agentTypeVerified == true
};
```

`{tenant}` is the tenant being bootstrapped. Unverified principals and
non-operator agent types remain default-deny for `manage_policies`.

The `when`-clause body is no longer written out here by hand. It is built
from a single shared constant, `temper_authz::VERIFIED_OPERATOR_WHEN`
(`crates/temper-authz/src/engine/mod.rs`), which
`operator_manage_policies_cedar` interpolates. The same constant is the
source of the operator clauses in the ARN-255 system-platform gate, so the
two surfaces cannot drift to different strength. The shared form adds `has`
guards (`principal has agent_type && … && principal has agentTypeVerified &&
…`) so a principal lacking either attribute evaluates to a clean deny
instead of a Cedar evaluation error; this is not a behavior change for the
operator, which always carries both attributes.

**Why this approach**: it is the smallest Cedar that closes the approval
loop, matches the existing HTTP authorization test, and does not grant
entity actions or `create_tenant`.

### Sub-Decision 3: Merge live Cedar; persist a stable granular row

1. Read the tenant's current live policy text.
2. Append the permit only if that statement is not already present
   (ADR-0144-style idempotent merge). Do not replace app Cedar.
3. Reload the tenant Cedar engine with the merged text.
4. Persist the **isolated** statement as granular policy id
   `operator-bootstrap-manage-policies` via `persist_and_activate_policy`
   (`created_by = "bootstrap"`). Hash-gated writes make re-bootstrap a
   no-op once the row exists.

Restart recovery (`recover_cedar_policies`) already concatenates granular
rows, so the permit survives reboot without a special case.

**Why this approach**: OS-app install already merges into live Cedar and
persists per-file rows. Reusing that pattern keeps operator bootstrap on
the same storage and activation path. Persisting only the statement (not
the whole live blob as `primary`) avoids wiping or duplicating app policy
rows.

### Sub-Decision 4: `create_tenant` stays a separate door

`create_tenant` is not required to list policies, add a Cedar rule, or
approve a denial in the bootstrapped tenant. This ADR does not grant it.
If a later loop is blocked on tenant provisioning, that is a different
permit and a different decision.

### Sub-Decision 5: Denied principal cannot resolve their own decision

After `manage_policies` succeeds, `POST .../decisions/{id}/approve` and
`POST .../decisions/{id}/deny` compare `security_context.principal.id` to
`PendingDecision.agent_id`. On match, return 403 and do not change the
decision or Cedar.

This is not Cedar. An agent who was granted `manage_policies` still cannot
approve themselves into power. A verified operator who is **not** the
denied principal can approve.

**Why this approach**: the missing operator door and the self-approval
ban are different failures. Cedar grants the governance door; the
approve/deny handlers refuse to let the subject of the denial walk
through it for that decision.

### Boundary with the ARN-255 identity-entity gate

Two operator-authorization surfaces coexist, and they are deliberately
different kinds of thing:

- **`PolicySet` / `manage_policies` (this ADR)** — a **tenant-seeded,
  overridable governance surface**. It is persisted as a granular Cedar row,
  merged into the tenant's live policy set, and can be disabled or replaced
  like any other tenant policy. It gates the governance loop (approve/deny,
  add Cedar), not the identity entities.
- **`TrustedIssuer` / `PrincipalGeneration` (ARN-255, #407)** — a
  **code-embedded, non-overridable platform-security surface**. Its permit and
  forbid clauses live in the built-in `system_platform_policy` in temper-authz
  and are merged into every engine at construction; a tenant cannot weaken
  them. They guard the god-mode identity entities (registering a signing
  issuer, bumping a principal's token generation).

Both now evaluate the **same** verified-operator predicate
(`temper_authz::VERIFIED_OPERATOR_WHEN`). Before this change the ARN-255
clauses checked only `agent_type == "operator"` while this ADR's permit also
required `agentTypeVerified == true`; a credential-unverified context that
merely self-declared `agent_type == "operator"` (the shape produced by
`SecurityContext::with_agent_context` on the trigger-dispatch path) therefore
passed the identity-entity gate but not this one — a fail-open. Unifying the
predicate and tightening ARN-255 to require verification closes that gap
without touching the authorization server, which is credential-resolved and
always `agentTypeVerified == true`.

Note that `"operator"` is a privileged claim ultimately guarded by
trusted-issuer registration: a registered issuer could mint a JWT carrying
`agent_type == "operator"`, but registering an issuer is itself a
`TrustedIssuer` write, gated to System/Admin/verified-operator by exactly
these clauses. The trust root is the issuer registry, not the claim.

## Rollout Plan

1. **Phase 0 (this PR)** — ADR, seed in `bootstrap_operator_credential`,
   self-resolution reject on approve/deny, red-green tests, live local
   `temper serve` on an isolated virgin store.
2. **Phase 1** — None required. Existing tenants pick up the row on the
   next process that runs `bootstrap_operator_credential`.

## Consequences

### Positive

- A verified operator can approve denials and manage Cedar on a virgin
  store without a hand-seeded policy file.
- The permit is ordinary Cedar: visible, persistable, and disableable
  like any other granular row.
- App Cedar remains intact across bootstrap, install, and restart.
- The denied agent cannot close their own governance loop, even with
  `manage_policies`.

### Negative

- A stolen `TEMPER_API_KEY` that resolves as the verified operator can
  manage policies for that tenant. That is the same trust already placed
  in the bootstrap key for identity; this ADR only makes the intended
  governance door reachable.

### Risks

- Re-bootstrap must not append forever. Mitigated by exact-statement
  merge plus hash-gated `save_policy` on a stable policy id.
- Loading only persisted rows after seed (and before app rows exist)
  would drop in-memory app Cedar. Mitigated by merging into live text
  and never calling `load_and_activate_tenant_policies` as the seed
  path.

### DST Compliance

- `persist_and_activate_policy` already uses `sim_now()` for trajectory
  timestamps.
- No new `HashMap`/`HashSet`, threads, or wall-clock in simulation-visible
  crates beyond existing persistence helpers.

## Non-Goals

- Skipping Cedar on approve or policy writes.
- Seeding `permit(principal, action, resource);`.
- Treating boot-time Cedar skip / `--app` process load as the fix.
- Granting `create_tenant`.
- Auto-registering the deployment key in every tenant (ADR-0157).
- Changing OS-app Cedar or named-action permits.

## Alternatives Considered

1. **Code bypass for `manage_policies` when principal is operator** —
   Rejected. Authority would not be Cedar, would not persist as a policy
   row, and would be invisible to the policy API.
2. **Permit-all for the operator** — Rejected. The bug is the governance
   door, not missing entity actions. OS-app Cedar already covers named
   actions it intends to allow.
3. **Document that operators must load a policy file** — Rejected. A
   virgin store cannot approve the first denial; the operator cannot
   write the file through the API that the file is meant to unlock.
4. **Grant `create_tenant` in the same seed** — Rejected. Separate door;
   not required for approve / add Cedar in the bootstrapped tenant.
5. **Rely on default-deny so the denied agent cannot approve** —
   Rejected. That fails as soon as they obtain `manage_policies`. The
   ban is on the approve/deny path.

## Rollback Policy

Delete or disable the `operator-bootstrap-manage-policies` row and restart,
or stop calling `bootstrap_operator_credential`. The credential entities
remain; only the Cedar door is removed.
