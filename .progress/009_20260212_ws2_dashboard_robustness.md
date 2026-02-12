# WS2 — Observability Dashboard Robustness

**Created**: 2026-02-12
**Status**: Seed Plan
**Team**: 1 agent (frontend focused)

## Goal
Transform the observability dashboard from a mock-data prototype into a production-ready tool that connects to the live Temper server, handles errors gracefully, and has test coverage.

## Current State

- **Tech stack**: Next.js 15, React 19, TypeScript, Tailwind CSS 4
- **5 pages**: Dashboard, Spec Viewer, Verification Runner, Entity Inspector, Layout+Sidebar
- **5 components**: Sidebar, SpecCard, StateMachineGraph, CascadeResults, EntityTimeline
- **API client**: 5 async functions calling `/observe/*` endpoints, ALL fall back to mock data on ANY error
- **Mock data**: `lib/mock-data.ts` has 3 fake entity types (Ticket, Invoice, Order) with full datasets
- **Tests**: Zero
- **Error handling**: None (mock data masks all errors)

## Phase 1: Remove Mock Data (P0)

This is the single highest priority item. The mock data masks real API failures and shows fake entities that don't exist.

### Step 1: Remove mock-data.ts
- Delete `observe/lib/mock-data.ts`
- Keep the TYPE DEFINITIONS — move `SpecSummary`, `SpecDetail`, `EntitySummary`, `VerificationResult`, `EntityHistory`, `EntityEvent`, `VerificationLevel`, `SpecAction`, `SpecInvariant`, `StateVariable` to a new `observe/lib/types.ts`
- Remove `import type { ... } from "@/lib/mock-data"` from all files
- Replace with `import type { ... } from "@/lib/types"`

### Step 2: Fix API client
- Remove all `catch { return MOCK_*; }` fallbacks in `observe/lib/api.ts`
- Replace with proper error propagation:
  ```typescript
  export async function fetchSpecs(): Promise<SpecSummary[]> {
    const res = await fetch(`${API_URL}/observe/specs`, { cache: "no-store" });
    if (!res.ok) throw new Error(`Failed to fetch specs: ${res.status}`);
    return res.json();
  }
  ```
- Each page should handle errors in its own loading state

### Step 3: Add error states to all pages
- Dashboard (`app/page.tsx`): Show error message if specs/entities fail to load
- Spec viewer: Show "Spec not found" or connection error
- Verification: Show verification error vs connection error
- Entity inspector: Show "Entity not found" or connection error
- Use a shared `ErrorDisplay` component

### Step 4: Update StatusBadge
- The `StatusBadge` in `app/page.tsx` has hardcoded colors for mock entity states (Open, Draft, Sent, Paid, etc.)
- Make it dynamic: generate colors based on state name hash, or use a small set of rotating colors
- Keep explicit mappings only for universal states (e.g., Done=green, Cancelled=red)

## Phase 2: Loading & Empty States

### Step 5: Loading skeletons
- Replace "Loading..." text with skeleton UI (pulsing gray boxes matching layout)
- Apply to: stats row, spec cards grid, entity table, spec detail, timeline

### Step 6: Empty states
- Dashboard with no specs loaded: "No specs loaded. Start the Temper server with `temper serve --specs-dir <path>`"
- Entity list empty: "No active entities. Create one with `POST /tdata/{EntitySet}`"
- Verification page with no results: "Click 'Run Verification' to start"

## Phase 3: Real-Time Updates

### Step 7: Auto-refresh
- Dashboard should poll `/observe/entities` periodically (every 5s) to show live entity state changes
- Use `setInterval` + `useState` or SWR/React Query for data fetching with automatic revalidation
- Add visual indicator: "Last updated: X seconds ago"

### Step 8: Connection status indicator
- Show a small indicator in the sidebar: "Connected to localhost:3000" (green) or "Disconnected" (red)
- On disconnect, show banner: "Cannot reach Temper server at {url}"

## Phase 4: Dashboard Features

### Step 9: Dynamic navigation
- Sidebar currently has hardcoded nav items
- Fetch spec list on mount, populate sidebar dynamically: one entry per entity type
- Show entity count badge next to each type

### Step 10: Entity filtering and search
- Add filter controls to entity table: filter by type, filter by state
- Add search box for entity ID
- Apply `$filter` and `$top`/`$skip` query params if server supports them (WS1 dependency)

### Step 11: Multi-tenant support
- Add tenant selector dropdown in header
- `SpecSummary` already has a `tenant` field — display it
- Filter entities by selected tenant

## Phase 5: Tests

### Step 12: Set up testing framework
- Install vitest + @testing-library/react + jsdom
- Configure in `vitest.config.ts`
- Add `test` script to `package.json`

### Step 13: Component tests
- `SpecCard`: renders spec summary, links to detail page
- `StateMachineGraph`: renders SVG with correct number of nodes/edges
- `CascadeResults`: renders pass/fail correctly, expandable details
- `EntityTimeline`: renders events in chronological order
- `StatusBadge`: renders correct colors for known states, fallback for unknown

### Step 14: API client tests
- Mock fetch, test each API function
- Test error handling (network error, 404, 500)
- Test that types match expected shape

### Step 15: Page integration tests
- Dashboard: renders stats, spec cards, entity table
- Spec viewer: renders state machine graph, actions table
- Verification: runs cascade, shows results

## Phase 6: Error Boundaries & Resilience

### Step 16: React error boundaries
- Add error boundary wrapper around each page
- On error: show "Something went wrong" with retry button
- Log errors to console for debugging

### Step 17: Retry logic
- API client: add automatic retry (1 retry with 1s delay) for transient failures
- UI: add "Retry" button on error states

## Acceptance Criteria

- [ ] `lib/mock-data.ts` deleted — no mock data anywhere
- [ ] Types extracted to `lib/types.ts`
- [ ] All pages show proper error states when server is down
- [ ] All pages show proper empty states when no data
- [ ] Dashboard auto-refreshes entity list
- [ ] Connection status indicator in sidebar
- [ ] At least 15 tests (components + API + 1 page integration)
- [ ] Error boundaries on all pages
- [ ] Dynamic sidebar navigation from live spec list

## Dependencies

- WS1 Phase 1 (OData fixes) improves the API quality but is NOT a blocker — the dashboard should work with current API as-is
- WS3 (runtime observability) will add new data sources the dashboard can display later
