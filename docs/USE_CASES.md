# Temper Use Cases & Analysis

A structured analysis of where Temper fits, where it could fit with extensions, and where it fundamentally doesn't apply — examined through concrete use cases in an agent-mediated world.

---

## Table of Contents

1. [What Temper Is](#what-temper-is)
2. [The Agent-Mediated Paradigm](#the-agent-mediated-paradigm)
3. [What Works Today](#what-works-today)
4. [Use Case Analysis](#use-case-analysis)
   - [Agentic Jira](#agentic-jira)
   - [Deep Sci-Fi (Agent Platform)](#deep-sci-fi-agent-platform)
   - [Life OS / Benji](#life-os--benji)
   - [Mission Control for OpenClaw](#mission-control-for-openclaw)
5. [The Agent's Perspective: Building With vs Without Temper](#the-agents-perspective-building-with-vs-without-temper)
6. [Fundamental Capabilities and Limitations](#fundamental-capabilities-and-limitations)
7. [Gaps That Can Be Filled](#gaps-that-can-be-filled)
8. [Integration with Entire.io](#integration-with-entireio)
9. [Connecting Claude Code to Temper](#connecting-claude-code-to-temper)

---

## What Temper Is

Temper is a research project exploring one hypothesis: most enterprise SaaS backends are state machines, and if you describe them formally, infrastructure (APIs, persistence, authorization, observability) can be derived mechanically.

**The core model:** Discrete states + integer counters + booleans, with transitions guarded by conditions on those three things, verified by exhaustive state space exploration.

**What exists today (392 tests, 0 failures):**

| Layer | What it does |
|-------|-------------|
| Spec parsing | Parses `.ioa.toml` files into structured specs with states, guards, effects, invariants |
| Verification cascade | 3-level verification: SMT (Z3), model checking (stateright), property testing (proptest) |
| Deterministic simulation | Reproducible simulation with fault injection (message delays, drops, actor crashes) |
| Actor system | Entity actors that enforce guards, reject invalid actions, process valid ones |
| JIT transition tables | Compiles specs into fast lookup tables, supports hot-swap |
| OData API | HTTP interface for entity operations |
| Event sourcing | Postgres persistence with full audit trail |
| Cedar authorization | ABAC policy evaluation |
| OTEL telemetry | Spans + trajectory tracking |
| Multi-tenancy | Tenant-isolated spec registries |
| Deploy pipeline | Verification-gated deployment |
| Evolution engine | O-P-A-D-I record chain for production feedback |

**The DST harness is fully generic.** Any valid `.ioa.toml` spec can be simulated immediately — no per-spec code needed. The framework discovers valid actions from the spec at runtime and explores all reachable states with fault injection.

---

## The Agent-Mediated Paradigm

The analysis below assumes an agent-mediated world where:

- **Humans interact through agents** (natural language in, agent interprets intent, calls APIs)
- **Agents generate UI** on the fly (A2UI pattern — no pre-built frontend needed)
- **Agents handle intelligence** (NL understanding, data validation, recommendations, content)
- **Temper handles rules** (lifecycles, guards, invariants, state enforcement)
- **Content/data rides along as params** (JSON in action parameters, stored but not verified)

This paradigm eliminates many traditional app-building concerns:

| Traditional concern | Who handles it in agent-mediated world |
|--------------------|---------------------------------------|
| UI / forms / dashboards | Agent generates on the fly |
| Search / filtering | Agent interprets queries, calls OData |
| Notifications | Agent observes events, tells relevant parties |
| Automation ("when X, do Y") | Agent watches for transitions, acts |
| Data validation | Agent validates before calling API |
| Bulk operations | Agent calls API N times |
| User preferences | Agent manages in its own context |

**The clean split:** Temper owns rules and lifecycles. The agent owns intelligence and interface. Neither tries to do the other's job.

---

## What Works Today

The fully generic pipeline, proven by 392 passing tests:

```
.ioa.toml spec
  → TransitionTable::from_ioa_source()     // parses ANY valid spec
  → Verification cascade                    // SMT + model checking + property tests
  → SimActorSystem::run_random()            // DST with fault injection
  → EntityActor with guard enforcement      // runtime state machine
  → OData API + Postgres + OTEL            // infrastructure
```

**What's not built:** Conversational spec generation (the interview flow), event subscription for agents, transition hooks/side effects, cross-entity orchestration, temporal guards.

---

## Use Case Analysis

### Agentic Jira

**Scenario:** A Jira-like issue tracker where both agents and humans interact through an agent interface.

**What maps to Temper:**

| Jira concept | Temper equivalent |
|-------------|------------------|
| Issue workflows (Open → In Progress → Done) | Entity spec — the sweet spot |
| Configurable workflows per project | Different specs per tenant via SpecRegistry |
| Transition rules ("only assignee can move to In Progress") | Guards + Cedar authorization |
| Transition validators ("must have estimate before Ready") | Guards: `guard = "is_true has_estimate"` |
| Multiple issue types (Bug, Story, Epic) | Multiple entity types, each with own spec |
| Activity log / audit trail | Event sourcing — every transition recorded |
| Sprint states (Planning → Active → Closed) | Another entity spec |

**What doesn't map:**

| Jira concept | Why |
|-------------|-----|
| Custom fields (text, dates, dropdowns) | Stored as unverified JSON in `fields` bag |
| Comments / attachments | Append-only data, not state transitions |
| Search / JQL | Agent translates queries to OData (basic), or queries directly |
| Automation ("when Done, move parent to Done") | Cross-entity — agent orchestrates, or needs choreography spec |
| Subtasks / parent-child relationships | Cross-entity invariants don't exist yet |

**In agent-mediated mode**, most "doesn't map" items are handled by the agent (search, notifications, automation, bulk operations). The remaining gaps are event subscription and cross-entity coordination.

**Temper fit: ~80%.** Issue workflows are the canonical state machine use case. Formal verification guarantees no issue can reach an invalid state.

---

### Deep Sci-Fi (Agent Platform)

**Scenario:** A crowdsourced platform where AI agents propose sci-fi worlds, validate each other's work, inhabit characters, and produce emergent stories. Humans consume the output.

**What maps to Temper:**

| Deep Sci-Fi concept | Temper equivalent |
|--------------------|------------------|
| Proposal lifecycle (Draft → Review → Approved) | Entity spec with guards and invariants |
| "Need N validations to approve" | Counter guard: `validations >= 3` |
| "Only agents with reputation > X can validate" | Cedar authorization policy |
| Dweller claiming (Available → Claimed → Active) | Entity spec |
| World lifecycle (Proposed → Active → Archived) | Entity spec |
| Story lifecycle (Draft → Published) | Entity spec |
| Action logging (what dwellers did) | Event sourcing |
| Reputation as gating mechanism | Counter on Agent entity + Cedar policies |
| Multi-phase pipeline with strict ordering | Guards enforce ordering |

**What doesn't map:**

| Concept | Why |
|---------|-----|
| Content itself (premises, critiques, stories) | Rich text — lives in JSON fields, unverified |
| Semantic search (pgvector) | Computational, not state-machine-shaped |
| Ranking / feed algorithm | Continuous scoring |
| "Causal chain must be scientifically plausible" | Requires LLM judgment — no formal guard for this |
| World canon (accumulated lore) | Growing unstructured document |

**Key insight:** Deep Sci-Fi already has a hand-written rules engine in Python doing what Temper does — but without formal verification. Rebuilding that rules layer on Temper gives you mathematical guarantees that no proposal can reach "Approved" without sufficient validations. DST would find edge cases like "what if two agents try to claim the same dweller simultaneously?"

**Temper fit: ~65%.** All lifecycle management and rule enforcement maps cleanly. Content, search, and ranking live outside Temper.

---

### Life OS / Benji

**Scenario:** A personal life organization system powered by agents. Multiple agents have access to your data sources (X, health apps, etc.), you dump information to them conversationally, and everything is organized into a "map of me" — mind maps of what you're working on, thinking about, with details on each.

**Initial assessment was 20% fit.** Re-evaluated in agent-mediated paradigm: **60-70% fit.**

**The shift in thinking:** In a traditional app, most of a Life OS is data entry forms + CRUD. In an agent-mediated app, the agent handles data entry and UI — what's LEFT is the rules and lifecycles. And that's what Temper does.

**Entities and their lifecycles:**

| Entity | States | Rules |
|--------|--------|-------|
| FocusArea | Exploring → Active → Paused → Completed/Abandoned | Can't exceed 5 active focus areas |
| Goal | Draft → Active → OnTrack/Stalled → Achieved/Abandoned | Can't achieve until all milestones done |
| Milestone | Pending → InProgress → Done | Blocked-by dependencies |
| Habit | Active → Paused → Archived | 3 consecutive misses → auto-pause |
| HabitDay | Pending → Completed/Missed/Skipped | Can't complete twice in one day |
| MedicationDose | Scheduled → Taken/Missed/Skipped | 3 consecutive misses → escalate |
| WorkoutSession | Planned → InProgress → Completed/Skipped | Can't complete without exercises logged |
| HealthTarget | Tracking → OnTrack/AtRisk → Achieved | Moves to AtRisk based on missed targets |
| Project | Idea → Planning → InProgress → Done/Shelved | — |
| WeeklyReview | Pending → InProgress → Completed | Can't complete without reviewing all flagged items |
| DataSource | Connected → Syncing → Paused → Disconnected | Can't sync with expired token |

**The interaction model:**

```
Human: "Had a rough week, skipped gym, ate badly,
        but I closed the freelance deal"

Agent decomposes into:

  DATA LAKE:
    → Store conversation, mood signal, nutrition signal

  TEMPER (verified state transitions):
    → HealthTarget('weight-loss'): LogMissedWorkout → AtRisk
    → Habit('gym'): MarkMissed × 5 → streak breaks
    → Project('freelance-deal'): CompleteDeliverable → Delivered
    → Goal('income'): UpdateProgress → counter increments

  AGENT INTELLIGENCE:
    → Reads state across all entities
    → "Weight loss goal moved to AtRisk. But freelance is
       delivered — 2/3 of income goal. Want to adjust gym
       schedule or pause the weight goal?"
```

**The "Map of Me":**

```
HEALTH [AtRisk]
  ├── Weight Loss Goal [AtRisk] — 5 missed workouts
  ├── Gym Habit [Broken] — streak was 12, now 0
  └── Sleep Target [OnTrack] — 7.2h average

CAREER [Active]
  ├── Income Goal [OnTrack] — 2/3 projects done
  ├── Side Project [Paused] — 2 weeks ago
  └── Learning: Rust [Active] — last study 3 days ago
```

Entity states from Temper. Content from data lake. Visualization generated by agent.

**What Temper handles:** Lifecycle rules, guard enforcement, audit trail, verified state transitions.

**What Temper doesn't handle:** Raw data storage (scraped tweets, health metrics, conversations), agent intelligence (interpreting natural language, generating UI, recommendations), data source integrations (X API, health apps), temporal rules ("stalled if no check-in for 7 days" — needs temporal guard extension).

**Temper fit: ~60-70%.** Strongest for the lifecycle/rules portion. The data lake and intelligence layers are built conventionally.

---

### Mission Control for OpenClaw

**Scenario:** Visibility into what OpenClaw agents are doing — especially when they spin up sub-agents (Claude Code, Codex) for coding tasks.

**Context:** Multiple OpenClaw Mission Control dashboards already exist ([crshdn/mission-control](https://github.com/crshdn/mission-control), [ClawDeck](https://github.com/clawdeckio/clawdeck), [ClawController](https://www.clawcontroller.com/)). They handle task management (Planning → Inbox → Assigned → In Progress → Review → Done), agent activity monitoring, and real-time logs via WebSocket.

**Where Temper could add value:**

| Concern | Temper value |
|---------|-------------|
| Agent lifecycle (Created → Running → Paused → Stopped) | State machine with guards — can't run without config |
| Task state tracking | Already handled by existing dashboards |
| Sub-agent spawning visibility | Could model as entities (CodingTask spec) |
| Incident management (Detected → Triaged → Resolved) | Good fit — lifecycle with strict rules |
| Access/permission management | Cedar authorization |

**Where Temper doesn't help:**

| Concern | Why |
|---------|-----|
| Real-time metrics (CPU, memory, request rates) | Continuous time-series data |
| Log aggregation | Unstructured streaming text |
| Alerting rules ("error rate > 5%") | Continuous threshold monitoring |
| Actual agent execution | That's OpenClaw's runtime |

**Assessment:** The existing Mission Control dashboards already cover task management and basic observability. The real gap is OpenClaw's event stream API (which is an [open issue](https://github.com/openclaw/openclaw/issues/6467)), not lifecycle management. Temper could be the state management backbone, but it's not where the primary value is for this use case.

**Temper fit: ~30%.** Most of what you want for Mission Control is already solved by existing tools or needs OpenClaw-side changes.

---

## The Agent's Perspective: Building With vs Without Temper

An honest comparison from the perspective of a coding agent (Claude Code) asked to build a Life OS.

### Without Temper

```
Agent picks a stack (FastAPI + Postgres, Next.js + Prisma, etc.)

Agent produces:
  ├── Database schema
  ├── API handlers with business logic in code:
  │     if goal.status != "active":
  │         raise HTTPError(400, "Only active goals")
  │     if goal.milestones_remaining > 0:
  │         raise HTTPError(400, "Complete milestones first")
  │
  ├── Similar handlers for every action on every entity
  ├── Error handling scattered across handlers
  ├── Tests the agent writes (maybe 50-100 cases)
  ├── Migration files
  └── Deployment config

Total output: ~2,000-5,000 lines of code
```

### With Temper

```
Agent interviews about the domain
Agent generates spec files

Total output: ~150-300 lines of TOML
+ Temper provides API, persistence, telemetry, auth for free
```

### Comparison

| Dimension | Without Temper | With Temper |
|-----------|---------------|-------------|
| What agent produces | ~3,000 lines of code | ~200 lines of TOML specs |
| Where bugs live | Scattered across handler code | In the spec only — verification catches them |
| Testing coverage | Agent writes tests for cases it thinks of | DST explores ALL reachable states with fault injection |
| Edge case: "complete abandoned goal?" | Only caught if agent thought to write that test | Impossible — `MarkAchieved` only allows `from = ["Active", "OnTrack"]` |
| Edge case: "milestones_remaining is 0 but not achieved?" | Agent might not think of this | Invariant checked after every transition; DST finds violations |
| Requirements change: "add Paused state" | Agent modifies 3-5 files, hopes nothing breaks | Agent adds to spec, verification confirms no regressions in seconds |
| Concurrent requests on same entity | Race condition unless agent explicitly handles it | Temper serializes transitions per entity — impossible to corrupt |
| What you get for free | Nothing — agent builds everything | OData API, Postgres event sourcing, OTEL, Cedar auth, multi-tenancy |
| Time to working API | 20-60 min of code generation | 5 min of spec writing + seconds of verification |
| 20th iteration of the same system | Accumulated complexity, buried bugs | Spec is still 200 lines, still fully verified |

### Honest agent self-assessment

**What agents are bad at when building backends from scratch:**
- Forgetting edge cases in guard logic
- Writing inconsistent rules across handlers (checking `!= "completed"` in one place and `== "active"` in another)
- Writing tests that cover the happy path but miss state combinations
- Handling concurrent access correctly
- Maintaining consistency across 20+ requirement changes

**What Temper changes for the agent:**
- The agent produces a SPEC (small, declarative) instead of CODE (large, imperative)
- Verification catches the agent's mistakes exhaustively
- DST finds edge cases the agent never thought to test
- Requirements changes are spec edits, not code surgery
- The 10x reduction in output surface area means 10x fewer places for bugs

**Where Temper doesn't help the agent:**
- Building the data lake / content storage
- Connecting to external APIs and data sources
- Agent intelligence (NL interpretation, recommendations, UI generation)
- Anything outside the lifecycle/rules domain

### When Temper's value compounds

Temper's advantage grows with **complexity and change**:

- **5 entities, stable requirements:** Agent can probably build it correctly without Temper. Nice to have, not critical.
- **15 entities, weekly iterations:** Temper is the difference between "this still works after 20 changes" and "something broke 3 changes ago."
- **Multiple agents building on the same system:** Temper guarantees structural correctness regardless of which agent made the last change.
- **Long-lived systems:** After 6 months of evolution, a conventionally-built backend accumulates tech debt. A Temper spec is still the same 200 lines, still fully verified.

---

## Fundamental Capabilities and Limitations

### Temper can do: anything where "correctness = valid sequences of operations"

The core question: **Can the correctness of this system be expressed as constraints on sequences of discrete operations?** If yes, Temper applies.

**Strong fit:**
- Business/productivity workflows (project management, CRM, ERP, HR, procurement)
- Healthcare (care episodes, prescriptions, clinical trials, claims)
- Legal/governance (case management, approval chains, regulatory filings)
- DevOps (CI/CD pipelines, incident management, change management)
- Agent-to-agent platforms (peer review, marketplaces, auctions)
- IoT device management (provisioning, firmware rollouts)
- Turn-based games (board games, card games, strategy)
- Financial workflows (loan applications, insurance claims, payment processing)

### Temper fundamentally cannot do: continuous computation, content manipulation, concurrent shared mutation

**Real-time physics/simulation:** Game engines, weather models, robotics controllers. Correctness = "physics equations solved accurately at 60fps." Continuous state, not discrete transitions.

**Content creation/manipulation:** Text editors, design tools, video editors. The artifact IS the value. Editing a paragraph is not a state transition. Collaborative editing needs CRDTs/OT.

**Real-time concurrent shared mutation:** Google Docs, Figma, multiplayer game worlds. Multiple users modifying the same data simultaneously with sub-second latency. Temper serializes transitions per entity — can't merge concurrent edits.

**Continuous computation as the product:** Spreadsheet engines, CAD solvers, compilers, recommendation engines, pricing optimizers. The value IS the computation.

**Stream processing:** Windowed aggregations over event streams (Kafka Streams, Flink). Different computational model entirely.

**Spatial/geometric reasoning:** Map routing, collision detection, 3D rendering, GIS queries. Continuous coordinates and distances.

### The hybrid reality

Most real apps are a mix. The question is: how much of the app's correctness is lifecycle-shaped?

| App type | Lifecycle % | Continuous % | Temper value |
|----------|-------------|-------------|-------------|
| Issue tracker (Jira) | ~80% | ~20% | High |
| Agent platform (Deep Sci-Fi) | ~65% | ~35% | High |
| Life OS (Benji) | ~60% | ~40% | Medium-High |
| E-commerce platform | ~50% | ~50% | Medium |
| Social media | ~30% | ~70% | Low-Medium |
| Collaborative design (Figma) | ~10% | ~90% | Low |
| Trading platform | ~15% | ~85% | Low |
| Game engine | ~5% | ~95% | Negligible |

**The principle:** If you can explain your app's correctness rules to a compliance officer using a flowchart with conditions on the arrows, those rules belong in Temper. Everything else belongs in the agent or conventional infrastructure.

---

## Gaps That Can Be Filled

None of the gaps identified below are fundamental architectural limitations. They're all "not built yet" — and in most cases, the skeleton exists in the code.

### Event Subscription

**What it is:** Agents need to know when transitions happen so they can react ("3 missed workouts → prompt user").

**What already exists:** Postgres event journal, Redis Streams (for actor mailboxes), OTEL WideEvents (emitted on every transition), IntegrationEngine with tokio mpsc channel.

**What's needed:** Expose entity events as a subscribable stream (Redis Streams fan-out or SSE endpoint).

**Effort:** Small. Events are already produced and persisted. Just needs a delivery channel.

**Verification impact:** None — read-only observation of verified transitions.

### Transition Hooks / Side Effects

**What it is:** Execute code after a transition (fire webhook, notify agent, update related entity).

**What already exists:** `Effect::Custom(String)` in specs, `[[integration]]` declarations parsed, `WebhookDispatcher` with retry logic, `IntegrationEngine` on mpsc channel, `dispatch_custom_effect()` stub function.

**What's needed:** Wire `EntityActor`'s transition result into the `IntegrationEngine` channel. Register hook handlers at startup.

**Effort:** Medium. Plumbing exists end-to-end. Connection points need wiring.

**Verification impact:** None — hooks fire after verified transition commits.

### Cross-Entity Orchestration

**What it is:** "When all subtasks are Done, move parent to Done."

**What already exists:** Multi-actor DST, Evolution Engine, SpecRegistry with all entity types.

**What's needed:** A choreography spec format + runner that subscribes to entity events and dispatches actions. Individual entity specs stay fully verified. Choreography gets DST exploration but not SMT proof.

**Important distinction:** Cross-entity *invariants* (verified combined state) hit state space explosion — this is a research problem. Cross-entity *orchestration* (event-driven coordination) is engineering — the agent or a choreography runner handles it.

**Effort:** Large but bounded.

### Temporal Guards

**What it is:** "If idle > 30 days, expire." "Stalled if no check-in for 7 days."

**What already exists:** `sim_now()` for deterministic time in simulation.

**What's needed:** Add clock/timer variables to spec format. Extend DST with time-based exploration.

**Effort:** Medium. The simulation already has deterministic time. The spec format and verification need to handle it.

### Append-Only Collections

**What it is:** Comments, attachments, change history.

**What already exists:** `EntityState.events: Vec<EntityEvent>` (bounded at 10,000), `fields: serde_json::Value`.

**Options:** Model each item as its own entity with trivial spec (works today), or extend spec format with a collection concept.

### Cross-Entity Queries

**What it is:** "Show me all issues assigned to me across projects."

**What already exists:** OData `$expand` is parsed. CSDL navigation properties are parsed. Neither is evaluated.

**What's needed:** Dispatch layer resolves navigation properties into additional actor queries.

**Effort:** Medium. Parsing is done. Evaluation isn't.

---

## Integration with Entire.io

[Entire.io](https://entire.io/) is Thomas Dohmke's (former GitHub CEO) new developer platform. $60M seed, $300M valuation. Core thesis: Git was built for humans; agents are now the primary code producers; the toolchain needs rebuilding.

### Entire's three-layer architecture

| Layer | What it does |
|-------|-------------|
| Git-compatible database | Stores code + the reasoning/context that produced it |
| Semantic reasoning layer | Humans and agents query WHY decisions were made |
| UI layer | Review, approve, deploy hundreds of AI-generated changes |

### First product: [Checkpoints](https://github.com/entireio/cli) (open source)

CLI tool that hooks into git workflow. When an AI agent writes code, Checkpoints captures the full session (prompts, responses, reasoning, intent, learnings). Stored on a separate git branch, keeping code history clean.

### How Entire and Temper complement each other

| | Entire | Temper |
|--|--------|--------|
| Focus | Development process — how code/specs get created | Runtime product — how to verify and run what was built |
| Question answered | "WHY was this built this way?" | "IS this correct?" |
| Artifact managed | Code + reasoning traces | Specs + verified state machines |
| Trust model | Transparency — see what the agent did | Verification — prove the result is safe |

**Together:** A verified spec AND the reasoning behind it, queryable by future agents and humans.

### The combined pipeline

```
Development time (Entire's domain):
  Developer describes app → Agent generates specs
    → Entire captures reasoning (WHY)
    → Temper verifies correctness (IS IT SAFE)
    → Developer reviews both → approves → deploy

Runtime (Temper's domain):
  Users interact through agent → Temper enforces rules
    → Events emitted → agent reacts
    → Unmet intents → Evolution Engine

Evolution (where they meet):
  Evolution Engine detects gap
    → Queries Entire's semantic layer for original reasoning
    → Proposes spec change with context
    → Temper verifies the change
    → Entire captures the evolution session
    → Developer reviews → approves → redeploy
```

**Entire provides the memory. Temper provides the guarantees.**

---

## Connecting Claude Code to Temper

### Vision

From PAPER.md: "The Developer Chat pipeline is partially implemented in `temper-platform` and **dependent on the coding agent of choice (claude-code, cursor, etc) at the agent integration layer.**"

The Developer Chat IS the coding agent. Temper doesn't need a custom conversational UI.

### What exists today

| Mechanism | Status |
|-----------|--------|
| Claude Code PostToolUse hook (auto-verifies on spec edit) | Working |
| `temper verify` and `temper serve` CLI commands | Working |
| Verify-and-deploy pipeline API | Working |
| PlatformEvent broadcast channel (verification status) | Working |
| AGENT_GUIDE.md (54KB reference for agents) | Exists |
| Interview flow / spec generation protocol | Not built |
| MCP server with structured tools | Not built |
| Skill with interview protocol | Not built |

### The right architecture: Skill + MCP

**Skill = the brain.** Knows HOW to interview, WHAT questions to ask, HOW to interpret errors in domain language. Guides the conversational flow. Cannot execute verification or deployment.

**MCP = the hands.** Executes verification, deployment, state queries. Returns structured data. Cannot decide what to ask the developer.

| Responsibility | Where | Why |
|---------------|-------|-----|
| Interview protocol | Skill | Conversational intelligence |
| Domain pattern recognition ("approval workflow" → states + guards) | Skill | Agent reasoning |
| Generating `.ioa.toml` from interview | Skill | Agent writes using format knowledge |
| Validating spec syntax | MCP | Mechanical: parse, check, return errors |
| Running verification cascade | MCP | Mechanical: run SMT + DST, return results |
| Translating verification errors to domain language | Skill | Intelligence: "tickets can reach Submitted with zero items" |
| Deploying specs | MCP | Mechanical: start server, return endpoints |
| Querying entity state at runtime | MCP | Mechanical: query actor, return state |
| Progressive disclosure (show/hide specs) | Skill | UX judgment |

**The flow:**

```
Developer: "I want a support ticket system"

SKILL activates → guides interview:
  "What states does a ticket go through?"
  "Any rules about who can close it?"

Agent generates spec files

SKILL instructs → MCP temper_verify():
  Returns structured results (pass/fail per entity per level)

SKILL interprets errors in domain language:
  "Reopening requires resolution, but resolutions are only
   set when closing. Allow reopening without resolution?"

Developer decides → agent fixes spec → MCP temper_verify() → passes

SKILL instructs → MCP temper_deploy():
  Returns endpoints

SKILL: "Ticket system is live at /tdata/Tickets"
```

### Build order

1. **Phase 1 (today):** Skill + existing hook + bash. Gets 80% of seamless experience.
2. **Phase 2 (soon):** Add MCP server. Structured tools replace bash. Richer error data.
3. **Phase 3 (later):** MCP makes Temper agent-agnostic (Cursor, Windsurf, any MCP client).
