# Implementation and acceptance plan

This records the plan accepted in the implementation conversation and captured
by [ADR-0178](../../adrs/0178-system-one-guards.md) before kernel changes. The
effort files were added later to supply the missing design chain required by CI.
The plan does not substitute for head-bound proof or an independent review record.

1. Extend the deterministic harness first with absent/malformed/stale evidence,
   negative and failed judgments, repeat attempts, tenant isolation, provider and
   storage faults, and changed state/specifications. Confirm that a missing
   production guard or unsupported-runtime check violates the invariant.
2. Add shared native question/request/answer types, the typed-list parser,
   recursive state bindings, static linting, fixed-point scalar comparison, and
   translation into the JIT and verification models. Keep ordinary guards
   compatible and kernel behavior independent of domain state names.
3. Add an injected bounded HTTP provider, tenant-secret access, immutable attempt
   reservations, durable receipts, and preflight context resolution. Persist both
   successful and failed outcomes before they can influence a transition.
4. Dispatch inference outside the actor critical section; construct private
   trusted evidence, then revalidate state/specification/authorization before
   effects and after OCC catch-up. Bind retry keys even when guards are removed
   or reordered. Include receipt references in committed events and reuse them
   during restart/replay.
5. Model answer outcomes as external inputs in the verification cascade; report
   abstraction limits. Explicitly refuse composite and Postgres-adapter execution
   surfaces that cannot yet enforce the same contract.
6. Verify production paths across many simulation seeds and real actors; run
   affected crate suites, L0-L3, formatting/lint/readability checks, and the full
   workspace suite. Record failures accurately and resolve their causes rather
   than weakening assertions or workloads.
7. Build and serve Temper locally. Drive Choice, Score, and Noul through OData
   using the user's ignored local credential imported into one isolated tenant.
   Exercise positive and negative actions, durable retries, changed-input
   conflicts, authorization rejection, and restart with the same journal.
8. Complete code/DST review, publish a PR, and attach real verification and review
   records bound to its final head. CI validators remain enabled; a local report
   or narrative claim alone is not a remote proof record.

## CI remediation

The first PR run rejected missing effort artifacts, a missing shaped decision
log, and missing current-head proof/review records. Supply this design chain and
the actual decisions in the PR. Fix concrete compiler, lint, or readability
failures found by the other checks. Refresh genuine evidence for the resulting
head and publish it through the trusted record producers. Do not label an
unsuccessful full-workspace run as passing or name reviewers that did not run.
