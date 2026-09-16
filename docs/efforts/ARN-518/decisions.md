# Decisions and tradeoffs

## D1 — Separate inline depth from background continuation hops

**Decision:** Preserve inline and reaction depth limits of 8 and add a finite 512-hop budget for each server-owned callback context lineage.

**Came up because:** The actual Foresight session completed a model/tool turn but its next background callback was refused by the shared eight-level depth counter. Main and the installed dependency are both bd15e89, so there is no newer fix to adopt.

**Options:** Increase the shared recursion limit; reset all counters at background dispatch; repeatedly resume stalled sessions; separate inline nesting from total callback hops.

**Chose separate budgets because:** Increasing the shared limit would also increase inline recursion, while resetting all counters would remove protection from background cycles. Separate counters allow finite asynchronous workflows without removing either bound. Only existing detached-task boundaries clear inline depth. No new scheduler, policy exemption, credential protocol, or app-specific logic is introduced.

**Where:** AgentContext and existing WASM/adapter/compensation spawn boundaries; docs/efforts/ARN-518/model.tla; regression and simulation tests. The guarantee is per internal context lineage, not aggregate fanout or restart-persistent resource accounting.
