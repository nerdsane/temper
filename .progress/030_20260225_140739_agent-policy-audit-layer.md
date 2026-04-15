# Plan 030: Agent Policy & Audit Layer

## Status: COMPLETE (pending commit)

## Overview
Implement the human-facing policy & audit layer: denial→surface→approve→reload loop + agent dashboard UX.

## Workstreams (Parallel)

### WS1: Backend Core (Phases 1-4) — backend-core agent
- [x] Phase 1: PendingDecision data model + bounded log
- [x] Phase 2: Denial interception at OData (trajectory + pending decision on 403)
- [x] Phase 3: Policy CRUD API (GET/PUT policies, POST rules)
- [x] Phase 4: Decision approve/deny API + Cedar generation + SSE stream

### WS2: Backend Observe (Phase 5) — backend-observe agent
- [x] Phase 5: Agent audit endpoints (list agents, agent history)

### WS3: Frontend (Phases 6-8) — frontend agent
- [x] Phase 6: Decisions page (pending cards, approve/deny with scope, history table, SSE)
- [x] Phase 7: Agents pages (list with stats + detail timeline with denial highlights)
- [x] Phase 8: Sidebar + types + API client

## Verification
- `cargo check --workspace` — PASS (0 errors)
- `cargo test -p temper-server` — PASS (all 22 tests)
- `npx next lint` — no new errors (pre-existing only)

## Key Files
- New: pending_decisions.rs, observe/agents.rs, decisions/page.tsx, agents/page.tsx, agents/[id]/page.tsx
- Modified: state/mod.rs, api.rs, observe/mod.rs, odata/bindings.rs, dispatch.rs, Sidebar.tsx, types.ts, api.ts

## DST Compliance
- BTreeMap everywhere, sim_uuid/sim_now, VecDeque bounded log, broadcast for SSE
