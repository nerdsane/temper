# Temper Meta

A working document for reading the kernel, and deciding what Temper v2 should be.

---

## Temper vision as I undertand it

Temper is a **universal constructor**: a machine that makes machines. You hand it a *description* and it verifies and executes it. The constructor stays fixed, descriptions change. 

The machines it makes are **joint cognitive systems**: humans and agents operating on shared state, with  actions checked before it happens and recorded after. 

This **enables directed evolution**, but evolution is not the kernel's job. The kernel's job ends at: verify, construct, run, record. Evolution would live outside and submit changed descriptions.

Machines built on Temper should come with **verification and deterministic simulation from the platform, by definition** - no CI, no external DST (no Antithesis); Temper replaces those, in the loop, as part of what deployed means.

---



## 1. What each crate does



### 1.1 What we want


| Concern         | Job                                                                      | Want it pluggable? | Can it, as built? |
| --------------- | ------------------------------------------------------------------------ | ------------------ | ----------------- |
| How defined     | Parse only. Skins over one IR. Today: IOA, CSDL, Cedar syntax.           | Yes                | No                |
| What is defined | `TransitionTable`, `Effect`, one `apply_effects`, what a log line means. | No                 | No                |
| How verified    | Cascade that calls that same apply.                                      | No                 | No                |
| Control plane   | HTTP, Cedar, registry, deploy, observe. May be several crates.           | No                 | No                |
| Interface       | How a person or agent talks to the kernel. CLI, MCP, SDK.                | Yes                | Yes               |
| Runtime         | Mailbox, timers, place, append/load.                                     | Yes                | No                |
| Arbitrary code  | Takes a custom name and becomes a WASM or adapter call.                  | Yes (the executor) | No                |
| Store           | The journal and platform tables.                                         | Yes                | No                |




### 1.2 Today’s crates, where they would belong


| Crate                   | Concern                                            | Expected                                                                                                                                               | Works                                                                                                                                                                                                                                                                                                                                                          | Doesn't                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| ----------------------- | -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `temper-spec`           | How defined                                        | Takes `.ioa.toml`, `model.csdl.xml`, `cross-invariants.toml` and becomes `Automaton`, `CsdlDocument`, `CrossInvariantSpec`.                            | Those three parse. `SpecModel` is CSDL + automata + a name check.                                                                                                                                                                                                                                                                                              | An `assert` it cannot parse is still a successful parse.                                                                                                                                                                                                                                                                                                                                                                                      |
| `temper-jit`            | What is defined                                    | Takes an `Automaton` and becomes a `TransitionTable`. Owns `evaluate_ctx`, `apply_effects`, `replay_effects`, `SwapController::swap`.                  | Server and verify call those. Swap replaces the table.                                                                                                                                                                                                                                                                                                         | `shadow_test` is not a gate. Swap does not migrate live entities.                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-verify`         | How verified                                       | Takes an `Automaton` and becomes a `TemperModel`. Runs L0–L3 + composite on the same `apply_effects`. Must not ship in serve.                          | Cascade is real and fast. A bad spec gets a counterexample.                                                                                                                                                                                                                                                                                                    | A rule it cannot check still counts as a pass. The machine it checks is smaller than the one that runs.                                                                                                                                                                                                                                                                                                                                       |
| `temper-authz`          | Control plane                                      | Takes a principal, action, and resource and becomes allow or deny. Default-deny.                                                                       | Named actions and writes are checked. Denials are recorded.                                                                                                                                                                                                                                                                                                    | -                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-odata`          | Control plane                                      | Takes a URL path and `$filter` and becomes a typed request.                                                                                            | Bound-action paths and `$filter` run.                                                                                                                                                                                                                                                                                                                          | `$filter` DoS, unbounded `$expand`, `$select+$expand` drops the join.                                                                                                                                                                                                                                                                                                                                                                         |
| `temper-observe`        | Control plane                                      | Takes a WideEvent and becomes an OpenTelemetry span.                                                                                                   | EntityActor and authz emit. `serve` starts OTEL export when an endpoint is set.                                                                                                                                                                                                                                                                                | WASM and LLM events skip the span. ClickHouse is not set up.                                                                                                                                                                                                                                                                                                                                                                                  |
| `temper-evolution`      | Should be outside of Temper kernel as a Temper app | Holds O–P–A–D–I record types.                                                                                                                          | Evolution entities are defined.                                                                                                                                                                                                                                                                                                                                | Evolution is an app on Temper. This crate is in the kernel. Temper enables it; it should not run it.                                                                                                                                                                                                                                                                                                                                          |
| `temper-ots`            | Should be outside of Temper as a format            | Holds an agent session as turns (messages, tools, results).                                                                                            | The JCS study converts a Claude session to ATIF and OTS and POSTs it. Server stores. Observe can list.                                                                                                                                                                                                                                                         | MCP’s built-in write is only Temper executes, not the session.                                                                                                                                                                                                                                                                                                                                                                                |
| `temper-platform`       | Control plane                                      | Boot a tenant (system + agent specs, operator key). Check a bearer. Verify-and-deploy. Install an app onto that tenant.                                | A tenant boots. A bearer is checked. `install_os_app` loads specs and Cedar.                                                                                                                                                                                                                                                                                   | App list is `os-apps/` in this repo.                                                                                                                                                                                                                                                                                                                                                                                                          |
| `temper-cli`            | Client                                             | `serve`, `verify`, `decide`, `mcp`.                                                                                                                    | `serve` starts the process. `verify` runs the cascade. `decide` writes an approval. `mcp` starts the stdio door.                                                                                                                                                                                                                                               | `serve` starts the kernel; the other commands are clients.                                                                                                                                                                                                                                                                                                                                                                                    |
| `temper-mcp`            | Client                                             | stdio `execute()` → HTTP.                                                                                                                              | An agent call becomes an HTTP request to `temper-server`.                                                                                                                                                                                                                                                                                                      | —                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-sandbox`        | Client                                             | Monty for the MCP door.                                                                                                                                | `execute` runs inside Monty.                                                                                                                                                                                                                                                                                                                                   | —                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-sdk`            | Client                                             | Rust HTTP client for `/tdata`.                                                                                                                         | A Rust caller hits `/tdata` over HTTP.                                                                                                                                                                                                                                                                                                                         | —                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-runtime`        | Runtime                                            | Bounded mailbox. Seeded clock and IDs. `EventStore` trait. `EntityRuntime` door.                                                                       | Mailbox send/recv. Tests get `sim_now` / `sim_uuid`. HTTP calls `InProcessEntityRuntime::execute()`.                                                                                                                                                                                                                                                           | Cannot swap the runtime. `EntityActor` is in server.                                                                                                                                                                                                                                                                                                                                                                                          |
| `temper-wasm`           | Arbitrary code                                     | Host executor: compile a leftover module, run it with fuel and memory, host HTTP / secrets / authz.                                                    | Server leftover calls this after the journal write. Fuel stop, memory cap, trap stays in the guest.                                                                                                                                                                                                                                                            | Not the runtime. Wired only inside server. Not on the journal.                                                                                                                                                                                                                                                                                                                                                                                |
| `temper-wasm-sdk`       | Arbitrary code                                     | Guest helpers for those modules (`temper_module!`, `Context`). Separate crate because the guest is `wasm32` and must not link the host engine.         | Modules use it.                                                                                                                                                                                                                                                                                                                                                | —                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `temper-actor-runtime`  | Runtime                                            | Same runtime job: mailbox, timers, place, append/load, behind `EntityRuntime`.                                                                         | `--actor-runtime postgres` can tell/ask and persist that mailbox in Postgres for a subset of specs.                                                                                                                                                                                                                                                            | Second ActorSystem, second mailbox, second spec apply. Not behind `EntityRuntime`. Default `serve` does not use it. Schedule, spawn, lists, triggers, WASM rejected. Process/ToolDefinition special-cased in server OData.                                                                                                                                                                                                                    |
| `temper-agents`         | Should be outside of Temper kernel as a Temper app | The agent loop as an app on EntityActor (`os-apps/temper-agent`).                                                                                      | —                                                                                                                                                                                                                                                                                                                                                              | This crate is a second loop in Rust (Process / LLM / tools on `temper-actor-runtime`). Only its tests use it. `serve` does not.                                                                                                                                                                                                                                                                                                               |
| `temper-server`         | Control plane + runtime + arbitrary code + store   | One process: HTTP / Cedar / registry / deploy / observe, the default `EntityActor` + journal, leftover, and store wiring.                              | A description can be served: named action, Cedar, evaluate, journal, leftover.                                                                                                                                                                                                                                                                                 | Those concerns are one crate, so none of them plug. Leftover is not in the journal. A second runtime (`--actor-runtime postgres`) sits beside the first.                                                                                                                                                                                                                                                                                      |
| `temper-dst`            | Tests                                              | Discrete-event run of the **production program** (mailbox, journal, leftover, HTTP). Simulated disk and network. Same seed, same trace. A deploy gate. | Three kinds of test, all in this crate. **Tick sim:** second scheduler, same `evaluate`, mailbox delay/drop/crash. **Harness:** production `install_os_app` / `dispatch_tenant_action` / `EntityActor` / recover on a fake store; Tokio; a seed. **Seeded persist tests:** same as the harness path, no discrete ticks — real `EntityActor` + `SimEventStore`. | Not FoundationDB/TigerBeetle: does not run `temper serve` under a fake NIC/disk; clock is not a process-wide virtual time that can jump; faults are `return Err`, often off before restart; no torn write, no crash mid-append. Tick sim is not production `EntityActor` (no journal, HTTP, Cedar, leftover). Harness and persist tests are seeded integration tests, not a sim of the server. No Postgres, HTTP, or WASM. Not a deploy gate. |
| `temper-store-sim`      | Store                                              | In-memory journal + platform + query. Seeded. Same contract as the durable store.                                                                      | DST append / replay. Seeded “return error.” Key and vector co-commit.                                                                                                                                                                                                                                                                                          | No query plane. Platform store lives in server. Faults are clean errors, not torn writes.                                                                                                                                                                                                                                                                                                                                                     |
| `temper-store-turso`    | Store                                              | One durable store: journal + platform + query.                                                                                                         | Local `serve` default. Append, replay, specs, policies, catalog, trajectories. Local file: WAL, 4 writers.                                                                                                                                                                                                                                                     | Remote: one writer. No key co-commit. Vector index write-behind. No data-only create. Second hand-written copy of Postgres.                                                                                                                                                                                                                                                                                                                   |
| `temper-store-postgres` | Store                                              | The durable store: journal + platform + query.                                                                                                         | A Postgres `serve` appends, replays, and keeps specs, policies, apps, catalog, keys, vectors, OTS, blobs here.                                                                                                                                                                                                                                                 | Hand-written copy of Turso. No shared conformance. Snapshot upsert does not check sequence.                                                                                                                                                                                                                                                                                                                                                   |
| `temper-store-redis`    | Store                                              | Mailbox, placement, cache for more than one node. Not a journal.                                                                                       | `--storage redis` can append and read events.                                                                                                                                                                                                                                                                                                                  | Mailbox modules were removed. No platform, no query. Multi-entity `append_batch` fails.                                                                                                                                                                                                                                                                                                                                                       |




### 1.3 Dependency order

1. `temper-spec` - how defined.
2. `temper-runtime` - mailbox, store trait, `EntityRuntime`.
3. `temper-jit` - table + the one apply.
4. `temper-verify` - cascade. Same apply. Never in serve.
5. `temper-authz`, `temper-odata` - Cedar and path.
6. `temper-store-sim`, then `temper-store-turso` - journal.
7. `temper-observe`. `temper-wasm` + `temper-wasm-sdk` (arbitrary code).
8. Other stores, `temper-ots`.
9. `temper-server` - now the parts exist.
10. `temper-actor-runtime` — second system, not a plug. `temper-agents` should not be here.
11. `temper-evolution`, `temper-platform`.
12. `temper-mcp` + `temper-sandbox`, then `temper-cli` / `temper-sdk`.



### 1.4 Lifecycle

A description is written, parsed, checked, loaded into a table, then used. 

```
write   agent / human
        .ioa.toml
        model.csdl.xml
        policies/*.cedar
        optional cross-invariants.toml
          |
          v
parse   temper-spec
        Automaton
        CsdlDocument
        CrossInvariantSpec
          |
          +---------------------------+
          |                           |
          v                           v
verify  temper-verify              load  temper-server registry
        Automaton → TemperModel          via temper-cli serve,
        same apply_effects as use        load-inline / load-dir,
                                         or temper-platform install_os_app
        temper-cli verify
        or temper-server                 lint errors block load
        POST /api/specs/validate-ioa     cascade does not
        L0–L3 + composite                load-dir streams cascade
                                         after the table is live
                                          |
                                          v
                                       Automaton → temper-jit TransitionTable
                                       Cedar     → temper-authz
                                       one CrossInvariantSpec per tenant
                                          |
          +-------------------------------+
          |
          v
use     named action: POST /tdata/Orders('o-1')/Temper.Vocab.SubmitOrder
        (bound actions need the namespace-qualified segment;
         the bare /Submit form parses as a navigation property → 405.
         Live-confirmed 2026-08-20.)

agent
  optional: temper-mcp execute()
        |
        v
HTTP  temper-server  (+ temper-platform bearer)
        |
        v
path  temper-odata
        |
        v
Cedar temper-authz
        |
        v
dispatch  EntityRuntime::execute()     (temper-runtime plug)
        |
        v
EntityActor  evaluate → apply_effects   (jit)
        |
        v
journal  EventStore                     (turso / sim / pg / redis)
        |
        v
after    schedule / spawn / WASM / observe
```



### 1.5 Files to open first

1. `test-fixtures/specs/order.ioa.toml`
2. `test-fixtures/specs/policies/order.cedar`
3. `crates/temper-spec/src/automaton/types.rs`
4. `crates/temper-jit/src/table/builder.rs`
5. `crates/temper-jit/src/table/evaluate.rs`
6. `crates/temper-jit/src/apply.rs`
7. `crates/temper-runtime/src/actor/traits.rs`
8. `crates/temper-runtime/src/plug.rs`
9. `crates/temper-server/src/entity_actor/effects/process.rs`
10. `crates/temper-server/src/entity_actor/actor/handle.rs`
11. `crates/temper-server/src/entity_actor/actor/persist.rs`
12. `crates/temper-odata` → `temper-authz` → `state/dispatch/actions.rs` → `post_dispatch.rs`

---



## 2. What works

- Hand Temper a description and you get a running app.
- Verification is real and fast enough to live in the edit loop - bad descriptions are rejected with the exact sequence that breaks them. Limitation: fails open on claims it cannot check, and the verified model is smaller than the machine that runs.
- The runtime enforces the machine with refusals that explain themselves.
- State is durable with full history and idempotent actions.
- A running app's behavior can be changed live, without restart or state loss. Limitation: safely only when the change adds; removals strand entities or lock the type.
- All state transitions governed by default and every denial is recorded.
- Part of the platform runs on itself - its own credentials, policies, and decisions are Temper entities.
- The kernel is deterministically simulated - seeded, reproducible, fault-injected. Limitations: coverage (only the actor and platform paths, against fake stores - no HTTP, WASM, or real storage), fault kinds (clean injected errors only - no torn writes, no real crashes or networks), and what it tests (faults are switched off before restart, so recovery under continuing failure is not exercised).

---



## 3. What doesn’t work

- The cross-entity check runs in `temper verify` but not on deploy or install, and a spec can go live before its verification finishes.
- When a spec changes, nothing migrates existing entities or their history onto the new state  machine.
- A spec change is verified on its own, not as a safe evolution of the version it replaces.
- The fault-injecting simulation the kernel tests itself with is a dev-time harness, so a deployed app's running system - its real stores, effects, and restarts - never gets simulated.
- Scheduled effects are never journaled, so a restart loses them, and idempotency guards the transition but not the effect, so an action can run its effect twice.
- Governance covers state transitions, but native adapters and config secret-templates run outside it and can reach secrets and outside hosts with no policy check.
- The store backends are written separately with no shared conformance suite, so they give different answers to the same query.
- Multi-node.
- Changes to the descriptions themselves - specs, policies, secrets - are not recorded as a log, so there is no history of how the machine's own rules changed.
- Runtime, control plane, and store are not pluggable, and in the actor runtime tenant isolation is a string convention.

---



## 4. Temper v2



### Did in v1 - do again in v2

- The description compiles to data the runtime interprets. A live swap is a pointer flip. Verification stays cheap because that data is the IR.
- Verification sits in the agent's edit loop. A bad spec gets a counterexample. The cascade replaces CI for that loop.
- A failed check returns a counterexample trace. A refused action says why it failed.
- Every action gets one check. The action is legal from this state. Cedar permits this principal. Default deny.
- A denial becomes a recorded decision. A human approves that decision with scope. The engine reloads the policy. The agent retries.
- Entities are event-sourced actors. Actions serialize. Facts go in the journal. Idempotency is built in. The journal is the audit.
- A change touches one entity type in one tenant.
- The platform runs on itself. The kernel stays small Rust. Tenants are separate from day one.



### Didn't do in v1 - do in v2

- Write the kernel promise as a Markdown spec and a TLA+-class formal spec before any implementation. A simulator runs that formal spec. Then write the kernel.
- The agent writes one artifact. The artifact compiles to one IR. The verifier, the runtime, the simulator, and the API generator read only that IR. The checker does not compile a second form.
- Skins over the IR can differ. A TypeScript eDSL is one skin. A P-syntax subset is another. An XState import proves that input can vary. Nobody authors a second artifact.
- Generate the HTTP API from the IR. That API is plain REST, OpenAPI 3.1, with x-temper extensions. MCP, the SDK, and the CLI are generated from that OpenAPI document. Nobody hand-writes a schema. Nobody implements a query language.
- The kernel writes its own verifier over the IR. A check finishes in milliseconds. Coverage is exhaustive. A later check is incremental and caches by content hash. The kernel does not take a checker from a dependency.
- Assume a spec is trying to sneak past the verifier. Mutation-test the spec. A green cascade is not proof.
- Every property says how sure we are. The property is proven, or the runtime enforces it, or the kernel rejects it. The kernel never trusts a property in silence.
- The kernel fails closed. Unknown syntax is a rejection. A claim the verifier cannot check is a rejection. An undeclared field is a rejection. A warning never lets a change through.
- Agents write the artifact. Humans can still read the artifact. Optimize the artifact for the writer first.
- Arbitrary code runs. The executor for leftover custom names is pluggable. WASM components are the default executor. TypeScript and Python compile to WASM. HTTP and secrets use the same host calls as named actions.
- One journal append commits the transition. That append includes entity state, timers, spawns, and leftover custom names. Either every one of those lands, or none of them does. Boot re-arms work that is still due.
- A workflow is an entity. Timers, child spawns, sagas, and human approvals are transitions on that entity. The kernel does not add a second engine for long-running work.
- Control plane and runtime are separate crates. The runtime is multi-node from the start.
- The journal is the write path. Reads go to rebuildable projections. Per-entity history is bounded. Audit and analytics read a columnar copy of the journal.
- A bundle is a content hash. Deploy flips a pointer to that hash. Rollback is the previous hash. Bundle history is a DAG.
- A description change ships a refinement diff. The diff says what became legal or illegal. The diff says which live entities would strand. The deploy rejects a stranding change until a converter exists.
- The journal has one schema. Observe, audit, and evolution are read models over that schema.
- The developer runs the simulator. The simulator gates deploy. The simulator uses a virtual clock, injects faults, and replays a seed. A failing seed blocks the description.
- Ship in the same tiers pstack uses. First, skills that analyze any repo. Then the gate as a library you add. Then the runtime. Each tier is useful before anyone converts a repo to Temper.

- A denial carries its remedy. The refusal names the policy that fired, what would pass - another principal, an earlier state, a granted decision - and the route to request it. The agent reads the 403 and knows its next move. Positional policy ids are the v1 anti-pattern. (ARN-403; diagnosability half is ARN-286/314.)
- Every app renders a policy sheet from the IR. One page: the state diagram, an action x principal x from-state table, each guard in one sentence, every integration and secret touched. Derived, never hand-written, so it cannot drift. Agents author apps; humans audit sheets, as easily as a Markdown file. `temper explain <app>` prints it. An undescribed guard fails the cascade. (ARN-404.)



### Did in v1, don't do in v2

- Four files you have to keep in sync, or a second grammar the agent also authors.
- OData. CSDL XML. A query language we implement and then have to defend.
- The same semantics written twice. A checker we did not write.
- The agent writes WASM. A compiler sits on the request path.
- Boot skips Cedar. The same description means different rules on different paths.
- The spec goes live. The cascade runs after.
- Reads replay the journal. Each action waits on its own append. Timers live only in memory, so a restart drops them.
- Four store backends, each written by hand.
- Evolution lives in the kernel on day one.



### Arbitrary code between transitions

- Arbitrary code runs between transitions. The machine sees the transition, not the code. v2 makes that code visible, bounded, and assured without forbidding it.
- Declare each unit's inputs, outputs, and effects in the spec. The boundary is checkable. The logic stays the unit's own.
- Bound what a unit can touch with declared doors, not ambient host access. Unverified code cannot exceed its declared reach.
- Assure each unit at the strongest tier it supports: proven, runtime-enforced, tested, or contained. Nothing sits at zero - a door and a journal are the floor.
- Route every effect through a door as deterministic journaled data. The unit replays. The simulator runs the real code under faults.
- Keep each unit a bounded leaf. The machine owns sequencing and external effects. A pile of steps in one body hides control flow from the check.
- A unit never dispatches transitions. If step B follows step A, that is two declared transitions, not a dispatch inside A's unit. One unit, one concern; a sequence inside one body is a spec smell the verifier rejects. (Rita, 2026-08-25; enforced by review in TemperPaw v1 until the kernel enforces it.)

---



## 5. pstack

- pstack is Lauren Tan's (@poteto, Cursor) public skill stack for agent engineering: ~20 skills, 22 playbooks, and 21 principles, installed as a plugin (github.com/cursor/plugins → pstack).
- What it does: routes every task through a playbook (`/poteto-mode`), farms signals via bot routines (Slack bug reports, X complaints, feature ideas) into an "outer loop", implements with cloud-agent swarms, and verifies with adversarial passes (`/swarm` fuzzing, `/arena` N attempts, `/interrogate` multi-model attack) before agents merge their own work; she reports 1,000–2,000 PRs a month.
- It is Temper-shaped (but it's all MD files + tests): two deterministic gates - the TypeScript compiler ("make illegal states unrepresentable" pushed into types) and tests/CI - and probabilistic everything else (playbooks executed by convention, multi-model consensus, screenshot and video evidence).
- Why it's similar to Temper: its principles are Temper's philosophy written as prompts - illegal states unrepresentable, prove-it-works against the real artifact, sequence verifiable units. This is validating.
- Devil's advocate objection: maybe nobody needs Temper - frontier models plus process rigor plus existing gates get most of the value, at prompt cost, on any codebase.
- Objection to the objection: the gates that actually bind are borrowed verifiers that only cover what types and tests can say; business-state rules have no compiler to lean on; everything ends at merge, so nothing governs runtime; at volume a small probabilistic miss rate is N number bad merges, critical systems can't tolerate any.
- UX learning for Temper v2: the form factor - start as installable skills inside existing tools, value before ritual (tier 0 analyze → tier 1 decorator gate → tier 2 runtime).

---



## 6. Thinking exercise - could Temper be assembled from existing blocks?

This row is Cloudflare only. Not our runtime. Not QuickJS.

The serve path can be Durable Objects, Cedar, and OpenAPI. Leftover runs on the Durable Object. That is the same Cloudflare block, not a second engine. Platform DST cannot. The production program is Cloudflare's. You can still write a simulator of a description over the IR. That is not the assembled platform.


| Piece                                                                          | Existing block                                                                                                              |
| ------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| Input language                                                                 | P subset, XState-style syntax                                                                                               |
| The IR and its single interpreter                                              | -                                                                                                                           |
| Millisecond in-loop verifier over the IR                                       | -                                                                                                                           |
| Deterministic simulator of a description (Temper app)                          | -                                                                                                                           |
| Platform deterministic simulation                                              | cannot. The production program is Cloudflare.                                                                               |
| The three gates: deploy, action, evolution (refinement diff, strand detection) | -                                                                                                                           |
| API description                                                                | OpenAPI 3.1, generated from the IR. CLI, MCP, and the SDK are generated from that document. MCP is a protocol, not a block. |
| Policy                                                                         | Cedar                                                                                                                       |
| Actors, leftover, storage, timers, placement, hibernation                      | Cloudflare Durable Objects                                                                                                  |




## 7. Spec for Temper v2


| Form              | Planned path                                                | Status       |
| ----------------- | ----------------------------------------------------------- | ------------ |
| Readable spec     | `docs/temper-v2/SPEC.md`                                    | Not written. |
| TLA+ / Stateright | `docs/temper-v2/temper.tla` (name TBD)                      | Not written. |
| Simulator         | New scaffold, from that spec, not a copy of today’s harness | Not started. |




### Decisions the spec must settle first (open as of Aug 2026)

1. **Frontend.** TS eDSL vs P-syntax subset (+ foreign TS functions). Same table underneath; decide by experiment, not taste - same three apps in each, count agent iterations-to-green and error classes.
2. **The mid-flight recovery line.** Transitions short + idempotent → event sourcing suffices (our model). Long imperative work that must survive crashes at arbitrary points → that is a durable-execution engine, a different machine. Where the line sits changes the spec’s core guarantee.
3. **What a transition commits.** Proposal on the table: state + timers + spawns + effects, one log append, atomic. Say it as an invariant in SPEC.md and let the simulator enforce it from day one.
4. **The assurance gradient contract.** Which properties may be ⚡ runtime-enforced vs must be ✓ proven, and how the tooling reports the tier. This is the spec’s user-visible promise.
5. **Substrate semantics.** The guarantees both runtimes (own Rust; workerd/DO later) must meet - effect delivery (at-least-once + idempotency keys?), timer resolution, swap semantics (drain at action boundary), what a deploy may and may not interrupt.
6. **What a self-hosting user with root still gets.** Enforcement is a custody property on every substrate; the honest answer today is “an excellent audit trail and a very good linter.” The spec should say which guarantees survive root and which don’t.

---



## 8. Dive in by crate

Rust under each crate. No `tests/`, no `*_test.rs` / `test_*.rs` / `tests.rs`. Counted 20 Aug 2026.

**Dive-in**  `█░░░░░░░░░░░░░░░░░░░░░░░`  11,410 / 185,617  (6.1%) - `temper-spec`, `temper-jit`

**Cleaned**    `██░░░░░░░░░░░░░░░░░░░░░░`    948 / 11,410    (8.3% of discussed)


| Crate                   | Lines       | Discussed  |
| ----------------------- | ----------- | ---------- |
| `temper-spec`           | 9,128       | yes        |
| `temper-jit`            | 2,282       | yes        |
| `temper-verify`         | 7,292       |            |
| `temper-authz`          | 2,930       |            |
| `temper-odata`          | 2,650       |            |
| `temper-observe`        | 3,314       |            |
| `temper-evolution`      | 3,617       |            |
| `temper-ots`            | 3,513       |            |
| `temper-platform`       | 16,342      |            |
| `temper-cli`            | 7,587       |            |
| `temper-mcp`            | 2,473       |            |
| `temper-sandbox`        | 2,330       |            |
| `temper-sdk`            | 698         |            |
| `temper-runtime`        | 4,769       |            |
| `temper-wasm`           | 9,264       |            |
| `temper-wasm-sdk`       | 1,633       |            |
| `temper-actor-runtime`  | 1,969       |            |
| `temper-agents`         | 1,693       |            |
| `temper-server`         | 81,042      |            |
| `temper-dst`            | 1,744       |            |
| `temper-store-sim`      | 1,043       |            |
| `temper-store-turso`    | 9,838       |            |
| `temper-store-postgres` | 7,621       |            |
| `temper-store-redis`    | 845         |            |
| **total**               | **185,617** | **11,410** |


**8.1** `temper-spec`

This crate only parses. It does not execute and it does not verify.

It takes `.ioa.toml` and becomes an `Automaton`.
`temper-jit` takes that `Automaton` and becomes a `TransitionTable`.
`temper-server` uses the table to run one action on one entity.
`temper-verify` takes the same `Automaton` and becomes a `TemperModel` for the cascade and for composite verify.

It takes `model.csdl.xml` and becomes a `CsdlDocument`. That is not an automaton.
`temper-server` uses it for `$metadata`, entity sets, types, and the relation graph.
`SpecModel` is the CSDL plus the automata, plus a name check.

It takes `cross-invariants.toml` and becomes a `CrossInvariantSpec`.
Not copied into the `Automaton`, the table, or a `TemperModel`.
`temper-server` keeps one per tenant: same entity name replaces; a second file replaces the whole spec, it does not merge.
`temper-jit` and `temper-runtime` do not use it.
`temper-server` checks it after a write; fail is 409.
`temper-verify` uses it on the joint `TemperModel`: fail if a matching action is enabled while the related field is wrong.
`temper-cli` `verify-ioa` does not load this file.

Cedar is not this crate.

**8.2** `temper-jit`

It takes an `Automaton` and becomes a `TransitionTable` (`from_automaton`).

The `Automaton` and the `TransitionTable` live in memory on the registry `EntitySpec`.
The TOML is the only durable form of that machine. It is in the database (`specs.ioa_source`). 

`evaluate_ctx` - can this named action fire in this status with this counters/bools/lists. Yes → effects. Guard fail → no effects. Unknown action → none.
`temper-server` `EntityActor` and composite dispatch call it.

`apply_effects` - writes status/counters/bools/lists through `EffectTarget`. Returns timers, spawns, emits, custom names. Does not run them.
`temper-server` `EntityActor` calls it on live `EntityState`.
`temper-verify` calls it on `TemperModelState`.

`replay_effects` - same effects, skips guards.
`temper-server` `EntityActor` `pre_start` calls it to rebuild `EntityState` from the journal.

`SwapController::swap` - replaces the table in the `RwLock`. Does not migrate live entities. Nothing in the repo does.
`temper-server` registry calls it on load-dir / re-register.

WASM is not this crate. `temper-server` runs leftover custom names via `temper-wasm`.

`shadow_test` exists. Not wired, and the implementation is questionable: it currently attempts to refuse if the state-machine shape/sequence changed, which would make state-machine changes impossible.

## 9. State of the DST (Temper v1)

Preliminary. Not FoundationDB / TigerBeetle DST.

**What those are.** One process runs the production program (`temper serve` as built). Disk and network are fake at the I/O door. Clock is virtual and can jump. A fault is “this write never landed” or crash mid-append. Same seed → same trace. That is running real code.

**A seed is not a simulation.** Same integer → same random choices. A Tokio test can be seeded. That does not make it discrete-event, and it does not fake the NIC or the disk.

`crates/temper-dst` holds three kinds of test. All three live in this crate. None of them is FoundationDB-style.

1. **Tick sim** (`SimActorSystem` + `EntityActorHandler`). Discrete ticks, like SimPy. A **second** scheduler — not the production `ActorSystem`. The handler is a **sync slice** of `EntityActor`: same `evaluate` / `apply_effects`, then stop. Mailbox delay / drop / crash. Spec invariants after each step. Same seed, same tick-sim trace. No journal, no HTTP, no Cedar, no leftover WASM. Leftover names are recorded and not run.
2. **Harness** (`SimPlatformHarness`). Calls production functions: `install_os_app`, `dispatch_tenant_action`, real `EntityActor`, Cedar recover, restore registry, rebuild index. “Restart” drops memory and rebuilds from the fake stores. Tokio is still Tokio. The I/O swap is only the store (`SimEventStore` + `SimPlatformStore`). Faults are `return Err`, and they are often switched off before restart.
3. **Seeded persist / index tests.** Same kind as (2): real `EntityActor` + `SimEventStore`, a seed, Tokio. No discrete ticks. Integration tests with a fake journal.

**What is real production code.** Both (1) and (2)/(3) share `evaluate` / `apply_effects`. (2) and (3) also run production `EntityActor`, persist, install, and dispatch — against a fake store, in a test process, not `temper serve`.

**What is not.** Production `ActorSystem` / HTTP / WASM / Postgres. Simulated NIC. Torn write. Crash mid-append. Recovery while faults stay on. A process-wide virtual clock. A deploy gate.

**Useful for:** spec invariants under reorder (tick sim); replay after a clean rebuild; tenant isolation; registry / Cedar / index after a harness restart; seed-reproducible install / dispatch.

Engines stay in runtime / store-sim / server. ADR-0168.

## 10. Notes

---

