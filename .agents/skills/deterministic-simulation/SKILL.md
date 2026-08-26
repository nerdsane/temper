---
name: deterministic-simulation
description: Deterministic simulation testing (DST), TigerBeetle/FoundationDB style, and DST-driven development. Use when building or testing a stateful system (Temper and anything like it - databases, queues, engines, protocol code), when a task mentions simulation, seeds, fault injection, or invariants, or before writing tests for concurrent/distributed logic. Not for frontend apps.
---

# Deterministic simulation testing

## What it is

The whole system runs inside a simulator that owns every source of nondeterminism: time, randomness, scheduling, network, disk. One seed drives one execution. The same seed replays the exact same execution, byte for byte. The simulator injects faults (crashes, partitions, delayed and dropped messages, disk errors) while invariants are checked continuously. Different seeds explore different executions; a failing seed is a permanent, replayable reproduction of a bug.

Reference implementations and what each proved:
- **FoundationDB**: a deterministic simulator running the whole cluster in one process; `BUGGIFY` markers in production code cooperatively inject faults with some probability when simulating; "swizzle-clogging" (clog a random subset of nodes' networks one by one, unclog in random order) finds the deep interleavings. Their bar: if production hits a bug the simulator could have expressed, that is a simulator gap to fix.
- **TigerBeetle (VOPR)**: an entire cluster of real code under network, storage, and process faults at ~1000x real-time (a virtual clock means simulated time runs as fast as the CPU allows); runs continuously across many cores and seeds; assumes the disk WILL fail - corruption and misdirected reads/writes are in the fault model, not just crashes.
- **Antithesis**: DST as a service over unmodified systems.

In this repo: the simulator lives in `temper-runtime` (sim module) with `temper-store-sim` as the simulated store; DST suites are `platform_e2e_dst` and `system_entity_dst` in `crates/temper-platform/tests/`.

## What it is NOT - the mistakes agents make

- **Not an integration test.** An integration test runs the system against real dependencies on real time and passes or fails once. A simulation runs thousands of seeded executions against simulated dependencies with faults injected.
- **Not a mock-based unit test.** Mocks replace the system's parts to isolate one piece. In DST the PRODUCTION CODE runs - all of it, unmodified. Only the environment (clock, network, disk, scheduler, entropy) is simulated.
- **Not a parallel reimplementation.** You do not write a second version of the logic and compare. The one real implementation runs in the simulator. A simplified MODEL may exist as an oracle to check results against, but the thing under test is always the production code.
- **Not "tests that use a seed."** If any nondeterminism leaks (a real clock read, an unseeded RNG, thread timing, iteration order of an unordered map), replay breaks and the whole method is void. Determinism is the load-bearing property.

## Requirements on the code under test

- All time via an injected clock. Never read the wall clock directly.
- All randomness from one seeded source the simulator provides.
- Single logical thread of execution, or scheduling fully controlled by the simulator.
- All I/O (network, disk, external services) behind interfaces the simulator can implement.
- No dependency on unordered iteration, real timers, or ambient environment.

If the code cannot meet these, that is an architecture finding to raise, not a reason to fall back to integration tests.

## DST-driven development

For systems like Temper this replaces test-driven development. The loop:

1. **Define the harness first.** Before implementing, extend the simulator with the scenario: the workload, the faults, and the invariants - the things that must never happen (lost write, double apply, stuck state machine, divergent replicas). Run it. **The invariant must fail now** - a harness that cannot catch the missing behavior proves nothing.
2. **Implement.** Production code, running inside the simulation.
3. **Run seeds until the invariants hold.** Not one seed - many. A green run on one seed is one execution, not correctness.
4. **A failing seed found later is committed as a regression case** and stays in the suite forever.
5. Fix by root cause. Never fix by weakening the invariant or narrowing the workload.

## Writing good simulations

- Coverage lives in the workload and fault schedule, not the framework. It is easy to build a simulator that explores almost nothing. Vary operation mixes, timings, fault frequencies; check that interesting states are actually reached.
- Invariants are properties, not examples: "no acknowledged write is ever lost", not "this call returns 3".
- Keep seeds cheap. A virtual clock costs nothing to advance - simulated hours run in wall-clock seconds. Fast executions buy more seeds per CI run, and more seeds are more coverage.
- Put cooperative fault points in production code (FoundationDB's BUGGIFY pattern): rare branches - a timeout firing early, a message reordered - taken with small probability only under simulation. The code helps the simulator find its own weaknesses.
- Report failures as: seed, invariant violated, minimal event trace. The seed IS the bug report.

## Limits - say them, do not hide them

DST cannot catch: bugs in the simulator's model of the environment, behavior of real external systems, real-clock/performance issues, and nondeterminism the harness failed to capture. Code changes invalidate old seeds' meaning (the seed replays a different execution). DST complements live verification; it does not replace the Definition of Done's live run.
