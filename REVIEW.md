# Reviewing temper

Use the installed Stack review contract, or the bundled [Stack review contract](.stack/REVIEW.md) when Stack is not installed. Review the accepted outcome and changed behavior; independently exercise the feature when useful. Report concrete defects introduced or worsened by this change, with reproduction evidence and location. No mandatory panel, review markers, JSON record or unrelated cleanup.

Apply these repository checks only where the change touches them:

- For simulation-visible changes, run the affected DST scenarios with reproducible seeds. Check simulated time, randomness, ordering and I/O; preserve failing seeds as regressions.
- Check that IOA invariants and runtime transitions agree, including replay and recovery. Keep application-specific state out of the kernel.
- Exercise changed authorization paths with allowed and denied principals and distinct tenants. Fail closed on missing policy or unverified identity.
- Check changed queue/buffer bounds, external-input error handling and dependency isolation (`temper-jit` must not pull verification tooling into production).
- Use the affected crate tests and `.agents/skills/verify-temper/` for the changed running flow.

Report what you tested, the revision, findings and material limits in plain language.
