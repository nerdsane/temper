# ADR-0177: Human-authorized MCP identity setup

- Status: Proposed
- Date: 2026-09-17
- Deciders: Temper maintainers
- Related: ADR-0173, ARN-467

## Context

A connector configured only with an operator credential requests work and
resolves approvals as the same principal. The server correctly rejects this
self-approval. Genesis has no separate registered requester. The working
TemperPaw installation was provisioned with a tenant identity-administration
policy and separate requester and operator credentials. Its setup was manual.

The owner authorizes a native chat setup operation. This is a new human
administration capability, not permission for execute code to set policies,
approve decisions, select arbitrary privileges, or impersonate another agent.

## Decision

Expose a separate, argument-free MCP setup operation. Its native elicitation
shows the configured server, tenant, fixed identity-administration grant,
requester identity, and local credential persistence. Only an affirmative
response delivered by the correlated MCP client request may authorize writes.
An execute expression, a tool argument, or a previous approval is not consent.

Use the configured service operator credential and existing policy and identity
administration endpoints. Resolve and verify that credential before offering
setup. Keep the self-resolution guard unchanged. The created requester uses a
fixed nonoperator type and receives no policy-management permission from setup.
Do not accept an endpoint, credential, policy, or privilege from tool arguments.

Persist the requester credential only in an explicitly configured private local
identity file, bound to the configured server and tenant. Do not expose tokens
in tool results, logs, trajectories, or source files. Reconnect loads and
revalidates that identity. Existing two-key configurations continue working.
Partial setup retains enough private state to reconcile rather than minting
another credential on a blind retry. Revoked identities are not reactivated.

## Contract

State sequence: Unconfigured -> AwaitingHuman -> Consented -> Provisioning ->
Verified -> Ready. Decline, cancel, disconnect, malformed response, wrong
correlation, or timeout cannot cross AwaitingHuman -> Consented. HTTP failure
cannot cross Provisioning -> Verified. Only a resolved nonoperator requester
distinct from the verified operator can cross Verified -> Ready.

No remote mutation or credential-file creation precedes Consented. No automatic
retry resolves an existing denied decision. The first subsequent governed
operation still uses the ordinary native approval flow.

## Verification and rollout

Test negative consent paths against a recording HTTP service and assert zero
writes. Verify endpoint/tenant binding, restart recovery, file privacy,
credential mismatch, revocation, partial failure, and distinct identities.
Exercise real stdio elicitation with a local Temper service; simulated client
responses are test fixtures only. Review before installing the connector.
Then obtain an actual human setup response separately for each authorized
service, verify identity readback and an actual governed approval, and test
local and Foundry callers. No live success claim before these observations.

## Alternatives and risks

Reusing the operator on both sides reproduces the defect. Removing the
self-approval guard undermines governance. Agent-authored policy strings or
automatic bootstrap would permit privilege escalation and are excluded.

The operator retains identity-administration authority after human consent;
the card must disclose that scope. Local credential storage needs private
permissions and concurrency protection. A lost human response is never inferred
from elapsed time. Rollback removes the new tool and retains the existing
working two-key mechanism; issued identities are revoked through governance.
