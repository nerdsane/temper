# Temper Verification Cascade

## Overview

Every spec change must pass the full verification cascade before deployment. The cascade runs automatically and its passage is a prerequisite for hot-deploying entity actors.

## Levels

### L0: SMT — Static Guard Satisfiability
- **What it checks**: Every guard expression in the spec is satisfiable (not contradictory)
- **What it proves**: No transition has an impossible guard condition
- **When it runs**: Immediately after spec generation, before any model checking

### L1: Model Check — Stateright Exhaustive Exploration
- **What it checks**: Exhaustive state space exploration via Stateright
- **What it proves**:
  - No dead guards (guards that are satisfiable but never reachable)
  - No unreachable states (states with no path from initial state)
  - All invariants are inductive (hold in initial state and are preserved by every transition)
- **When it runs**: After L0 passes

### L2: DST — Deterministic Simulation Testing
- **What it checks**: Single-entity behavior under deterministic simulation with fault injection
- **What it proves**:
  - Entity behaves correctly under simulated failures (message drops, reordering, delays)
  - State transitions are deterministic and reproducible
  - Invariants hold across simulated fault scenarios
- **When it runs**: After L1 passes

### L2b: Actor Sim — Multi-Actor Simulation
- **What it checks**: Multi-actor interactions with message passing under simulation
- **What it proves**:
  - Cross-entity interactions maintain consistency
  - Message ordering and delivery semantics are correct
  - System-level invariants hold across actor boundaries
- **When it runs**: After L2 passes, for specs involving multiple entity types

### L3: PropTest — Property-Based Testing
- **What it checks**: Randomized property-based tests for edge cases
- **What it proves**:
  - Transitions handle boundary values correctly
  - Guard conditions behave correctly at edges of their domains
  - No unexpected panics or invariant violations under random inputs
- **When it runs**: After L2/L2b passes

## Cascade Guarantees

When the full cascade passes:
1. Every guard is satisfiable and reachable
2. Every state is reachable from the initial state
3. All invariants are inductive and hold under fault injection
4. Multi-actor interactions are consistent
5. Edge cases are covered by property-based tests

The cascade runs before any spec is deployed. There are no exceptions.
