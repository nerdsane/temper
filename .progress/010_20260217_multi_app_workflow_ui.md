# Plan: Multi-App Loading + Temporal-Like Workflow UI

## Status: Complete

## Verification
- `cargo test --workspace` — all tests pass (zero failures)
- `cargo build` — clean build, no warnings
- `npx next build` — clean build, all routes generated
- New test: `test_workflows_returns_tenant_data` passes

## Phase 1: Backend — Multi-App Loading
- [x] 1.1 Add repeatable `--app` flag to CLI
- [x] 1.2 Modify `serve::run()` to load multiple apps
- [x] 1.3 Extend DesignTimeEvent with workflow metadata

## Phase 2: Backend — Workflow Event Log
- [x] 2.1 Add workflow event history to ServerState (design_time_log)
- [x] 2.2 New endpoint: `GET /observe/workflows`

## Phase 3: Frontend — Temporal-Like Workflow Page
- [x] 3.1 Add Workflow types and API functions
- [x] 3.2 Add WorkflowTimeline component
- [x] 3.3 Add Workflows list page
- [x] 3.4 Add Workflow Detail page (per-tenant)
- [x] 3.5 Update Sidebar with Workflows nav
- [x] 3.6 Wire up live SSE on Workflow Detail page
- [x] 3.7 Update Dashboard with app grouping links
