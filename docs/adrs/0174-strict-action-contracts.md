# ADR-0174: Strict action contracts

- Status: Proposed
- Date: 2026-09-06
- Related: ARN-467; docs/efforts/ARN-467/spec.md

## Context

Action guards previously lacked incoming parameters, and successful actions synchronized undeclared incoming fields. Generic writes could also change fields without a named transition. Applications separating intended and observed state therefore could not enforce that separation mechanically.

## Decision

An IOA can declare strict action parameters and explicit input constraints. One pure validator runs before effects in both native actors and simulation. Strict entities accept identity-only creation and require declared actions for later mutation and deletion. Parameter values never appear in validation errors. Stored transition tables retain the same contract on restart.

## Consequences

Applications gain an enforceable action vocabulary, at the cost of declaring every input and using actions to initialize fields. This does not validate provider evidence or grant authorization. Cedar still evaluates every dispatched action. Existing applications require an explicit specification change to adopt strict behavior.

## Verification and rollout

The same IOA contract is exercised by simulator, native actor and HTTP tests. Real multi-entity factory cascade tests use registry-derived reactions. The kernel is reviewed and deployed through the pinned TemperPaw dependency before strict DSF specifications are installed.
