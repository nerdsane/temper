# Temper Philosophy

## Core Beliefs

1. **Specs are generated from conversation, never hand-written** — Developers describe what they want through natural conversation. The system interviews them about entities, states, actions, and guards, then generates IOA specs, CSDL, and Cedar policies automatically.

2. **Code is derived from specs and is regenerable** — Specs are the source of truth. All runtime behavior (TransitionTables, entity actors, OData endpoints) is mechanically derived from the spec. If you lose the code, you regenerate it from the spec.

3. **The verification cascade gates every spec change** — No spec reaches production without passing all verification levels (L0 SMT, L1 Model Check, L2 DST, L2b Actor Sim, L3 PropTest). This is non-negotiable.

4. **Framework code must NOT hardcode entity-specific state names** — The platform is generic. Entity-specific knowledge lives exclusively in specs. Framework code operates on spec-derived structures (TransitionTable, state IDs, action IDs) without knowing what "Open" or "Closed" means.

5. **Domain invariants come from the spec's [[invariant]] sections** — Invariants are declared in the spec, not scattered through application code. The verification cascade and simulation runtime enforce them automatically.

6. **Trajectory intelligence captures every unmet intent** — When a user request cannot be fulfilled by the current spec, that gap is recorded as a trajectory span. These accumulate in ClickHouse, where the Sentinel detects patterns and proposes spec evolution.

7. **Two separated contexts: Developer Chat (design-time) and Production Chat (runtime)** — Developer Chat can modify specs and trigger redeployment. Production Chat operates strictly within the bounds of deployed specs. These contexts never cross.

8. **The developer holds the approval gate for all behavioral changes** — The Evolution Engine proposes changes (I-Records), but only a developer can approve them (D-Records). Autonomous spec mutation is not permitted.
