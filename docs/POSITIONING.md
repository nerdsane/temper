# Temper: From Agent-Built Tools to Verified Capabilities

## 1. Agents Are Starting to Build Their Own Tools

Agent scaffolding shrinks as models get smarter. Prompt templates, tool wrappers, output parsers: the model absorbs what used to require code around it. Two things remain. Agents need infrastructure to run on: filesystems, sandboxes, persistence, authorization. And agents need tools to do their work: trackers, pipelines, coordinators.

Agents are starting to build the second category for themselves. A coding agent generates an MCP server mid-session because the tool it needs does not exist. A planning agent synthesizes a workflow tracker to coordinate subtasks. An operations agent creates a notification pipeline to monitor deployments. These are not pre-built tools the agent was given. The agent decided it needed them and made them.

Most agents still operate with a fixed set of tools handed to them by a developer. As models get more capable, agents will build more of their own tools. The question is what happens to them.

## 2. Most Tools Are State Machines

Consider what agents tend to build. A project tracker with statuses: backlog, in progress, in review, done. A deployment pipeline: pending, building, testing, deployed, rolled back. A notification system: draft, scheduled, sent, failed, retried.

The same shape runs through enterprise SaaS. An e-commerce order moves through draft, submitted, confirmed, shipped, delivered. A support ticket goes from open to in progress to resolved to closed. A subscription cycles between active, past due, suspended, cancelled.

The core logic in each case is a state machine. Statuses, transitions between them, rules about which transitions are allowed ("you can't ship without confirming payment"), constraints that must hold ("cancelled is final"). The entities are different. The shape of the problem is the same.

This is the hypothesis Temper operates on. A large class of the tools agents build, and a large class of the applications developers build, share this structure. If the state machine is the essential artifact, two things follow.

First, verification becomes tractable. You can prove, before anything runs, that every rule is satisfiable, every constraint holds across all reachable statuses, and no failure scenario violates the contract. You cannot do this for arbitrary code in general. You can do it for state machines.

Second, the tools agents build are not code. They are *descriptions* of behavior: statuses, transitions, rules. This distinction matters for everything that comes next.

## 3. Descriptions Need a Constructor

A description sitting in a file does nothing. Someone has to read it and build the running system it encodes: the persistence layer, the API endpoints, the authorization checks, the event journal. If you build all of that by hand for each description, you have not gained much.

In 1949, Von Neumann asked what threshold of complexity a machine must cross before it can evolve. His answer was a machine with three parts: a *description* that encodes a blueprint, a *universal constructor* that reads any description and builds whatever it encodes, and a copy mechanism that duplicates descriptions. The constructor is generic. It does not know what it is building. It interprets whatever you feed it. Evolution happens by changing descriptions, not the constructor.

Temper follows this separation. The kernel is the constructor. It reads specifications and builds a running system from them: verification cascade, actor runtime, event sourcing, authorization engine, API generation. The kernel does not know whether you are building a project tracker or a deployment pipeline. It interprets whatever you feed it.

An agent that needs a project tracker writes a description of a project tracker. The kernel verifies the description, deploys it, and the agent operates through it. An agent that needs sprint planning writes a description of sprint planning. Same kernel, different description.

We call a verified, deployed description a *capability*. A capability bundles:

- A natural language description of the capability ("issue tracking with projects, cycles, labels, and comments") for discovery and indexing
- Guidance for agents on how to use the capability: patterns, examples, constraints
- One or more state machine specifications defining statuses, transitions, guards, and invariants
- A data model defining the entity schema
- Authorization policies defining who can do what
- Integration declarations for external systems (sandboxed)

The natural language description and guidance are what agents and humans read. The specifications are what the kernel verifies and executes.

## 4. Verified Capabilities Still Need Governance

A verified capability is correct: its state machine does what the description says. But the agent operating through it can still do things it should not. It can reassign every issue to itself. It can access another agent's project. It can call an external API through an integration without anyone approving that access.

Verification handles correctness. It does not handle who can use the capability, or how.

Temper uses a default-deny authorization posture. When an agent attempts an action that no policy permits, the denial surfaces to the human: "Your agent tried to reassign issues in Project X. Allow?" The human approves with a scope: narrow (this agent, this action, this resource), medium (this agent, this action, any resource of this type), or broad (this agent, any action on this resource type). Temper generates the authorization policy and hot-loads it.

The human does not write policies from scratch or anticipate what the agent will need. The human responds as needs arise. Over time, the policy set converges on what the agent requires.

## 5. Capabilities Must Evolve

An agent described a project tracker last week. This week the team needs sprint planning. The tracker's state machine does not cover it. Someone has to write a new description, re-verify, redeploy. Next month the team wants labels and priorities. Another description. The capabilities the agent built are frozen at the moment of creation.

Two paths address this.

**Agents create new capabilities.** When an agent encounters a problem the current capability set does not cover, it writes a new description. The kernel verifies and deploys it. An agent that needs sprint planning describes sprint planning.

**Existing capabilities evolve through use.** The kernel records every agent action as an entity transition. Separately, the MCP bridge captures each agent's full execution trace: what the agent tried to do, what succeeded, what failed, what it gave up on. The GEPA (Guided Evolution of Pareto-optimal Artifacts) replays these execution traces against the current specs, clusters failure patterns, and proposes changes to existing descriptions. Agents keep trying to assign issues to teams, but the tracker only supports individual assignees? The GEPA produces a spec diff that adds team assignment. The change goes through the verification cascade before it takes effect.

Both paths are evolution in Von Neumann's sense: changes to the descriptions, not the constructor. The kernel stays stable. The capabilities change.

## 6. Evolution Needs a Trust Gradient

If a human must approve every description change, the system cannot scale. If the system evolves autonomously, you lose control.

The answer is a spectrum. On one end, the human approves every significant decision. On the other end, the system operates within pre-approved boundaries. You move along this spectrum as trust builds, as the policy set matures and the execution traces give you confidence. The scope of autonomy expands within specific boundaries, backed by data.

That is the chain: **unverified → verified → governed → evolving → trust-calibrated**. Each step solves a problem the previous step could not. Each step depends on the one before it. Governance assumes verification. Evolution assumes governance. Trust calibration assumes evolution data.

## 7. Interpretability

A system that evolves its own descriptions raises a concern: changes humans cannot understand or control. Temper tries to address this at several points, from the spec format down to individual agent actions.

**The spec is the documentation.** In a traditional system, the code is the source of truth and documentation trails behind it. In Temper, the description is both. You can open a spec file and read the statuses, transitions, guards, and invariants. That file is what the kernel verifies and what the runtime executes. There is no separate implementation that could diverge from the spec.

**Verification produces counterexamples, not just pass/fail.** When the cascade finds a property violation, it returns a concrete trace: the exact sequence of actions that leads to the broken state. "If an agent calls Assign, then StartWork, then SubmitForReview, then ApproveReview without a reviewer assigned, the invariant 'reviewed implies reviewer exists' is violated." You debug at the domain level, not the code level.

**Every action is an event.** The kernel persists every entity transition as an immutable event: the action name, the agent that performed it, the before and after state, and the authorization decision that allowed it. You can reconstruct the full history of any entity from its event journal. You can answer "who moved this issue to Done, when, and which policy permitted it?"

**Every authorization decision is recorded.** Each allow or deny captures the policy that applied, the principal (which agent), the action, and the resource. Denied actions create pending decisions for the human. The authorization history shows how the policy set developed over time and why each permission exists.

**Agent trajectories are captured.** The MCP bridge records the agent's session trajectory: every tool call, every decision, every success and failure. The Temper-native agent captures even richer trajectories, since every agent action is itself a governed entity transition. These trajectories are stored separately from entity events. The GEPA replays them against current specs to find gaps. A human can inspect any agent session to understand what the agent attempted and where it got stuck.

**Evolution changes are traceable.** The O-P-A-D-I record chain (Observation, Problem, Analysis, Decision, Impact) connects every spec change back to the observation that motivated it. You can ask "why does this state exist?" and trace the answer: an observation from agent execution traces, the problem the GEPA identified, the analysis it performed, the decision a human approved, and the measured impact after deployment.

**The Temper-native agent takes this further.** When agents run as entities inside Temper, their state machines, budgets, and lifecycle are governed by the same kernel. Every agent action is an event. You can pause an agent, resume it from any point in its history, or replay its entire execution. The agent becomes as inspectable as the capabilities it operates through.

## 8. Where This Stands

Temper is version 0.1.0. The constructor works. The description format is stabilizing. The evolution loop runs end-to-end in testing. 950+ tests pass across 25 crates.

**The constructor can do this today.** Parse a description (I/O Automaton spec + data model + authorization policies). Run a four-level verification cascade. Deploy it as a live actor with event sourcing, a generated API, and authorization enforcement. Hot-reload when the description changes. Record every entity transition. Capture full agent execution traces through the MCP bridge. Run the GEPA against those traces to propose description changes. Enforce cross-entity invariants (both hard constraints and eventual consistency with bounded convergence). Surface denied actions to the human for approval.

**Three pre-built capabilities ship with the platform:** project management (5 entity types), filesystem (4 entity types), agent orchestration (3 entity types). Agents can install them, operate through them, and propose changes. Agents can also submit new descriptions.

**The constructor cannot do this yet.** No floating-point state variables (prices live in payload fields, not state). No conditional effects ("if items > 5 then discount" requires decomposing into separate guarded actions). Single-node only (Redis traits are designed but not wired). No temporal guards ("if idle > 30 days" requires scheduled actions or integration engine cron triggers). Some of these are fundamental to finite automata. Others are engineering work.

**Temper is being built bottom-up.** Each layer enables the next.

| Layer | What it does | Status |
|-------|-------------|--------|
| **6. Agent Execution** | Agents as entities with their own state machines, budgets, and lifecycle. | In Progress |
| **5. Pure Temper Agent** | Agent's only tool is Temper. Every action mediated. | In Progress |
| **4. Harness Composition** | Agents describe harnesses as specs. | In Progress |
| **3. Integration Framework** | External APIs as sandboxed WASM modules, governed by authorization. | In Progress |
| **2. Temper as Filesystem** | Entity persistence replaces markdown files and JSON blobs. | In Progress |
| **1. Capabilities** | Agents write descriptions. The kernel verifies and deploys them. Others consume them through the generated API. | Done |
| **Foundation: Kernel** | Spec interpreter, verification cascade, actor runtime, authorization, event sourcing, evolution engine. | Done |

Today, agents interact with Temper through an MCP bridge. The next layers close that gap: agents as first-class entities inside the constructor, then agents that compose harnesses as descriptions, then agents whose only tool is Temper itself. Whether this pattern holds across a broader set of real-world agent deployments is the next thing to find out.
