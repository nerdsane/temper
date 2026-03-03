# Temper as the Operating Layer for Autonomous Agents

## The Premise

Agents are getting good at acting. They can write code, call APIs, manage tasks, coordinate with other agents. What they lack is an operating layer — something that governs what they do, records what they've done, and ensures they can't silently break things.

Today, agents run with whatever tools they're given. They call APIs directly, write to databases, execute code in sandboxes. There is no shared governance model. There is no formal verification of what an agent is about to do. There is no audit trail that connects an agent's intent to its effects. When something goes wrong, you grep through logs.

Temper's thesis: **every state-changing action an agent takes should flow through a governed, verified, auditable layer.** Not optionally. By design. The agent is given a REPL interface to Temper as its tool for mutation — not raw API keys, not direct database access, not arbitrary code execution.

This is not a framework for building agents. It is the operating layer that agents run on top of.

## What This Means Concretely

### The Agent Is Both Developer and Operator

In the personal assistant and enterprise employee use cases, the agent builds its own specifications. When an agent needs to execute a multi-step plan — process an expense report, coordinate a deployment, manage a customer interaction — it generates an IOA specification describing the states, transitions, guards, and integrations of that plan. Temper verifies the spec through a four-level cascade (SMT symbolic checking, exhaustive model checking, deterministic simulation, property-based testing) before the agent can execute through it.

The agent then operates through the verified spec: calling actions, transitioning state, triggering integrations. The spec is the contract. The verification cascade is the proof. The runtime enforces the contract on every action.

This is different from "an agent calls some APIs." The agent's plan itself is a verified state machine. An agent cannot ship an order without payment captured — not because of a code review, but because the invariant was proven to hold across all reachable states before the spec was loaded.

### The Human Is the Policy Setter

The agent builds and operates. The human (or an oversight agent) governs.

Cedar policies define what agents can and cannot do. The default posture is deny-all. When an agent attempts an action that no policy permits, the denial is surfaced to the human: "Your agent tried to call the Stripe API and was blocked. Allow for this agent? This action? This resource type?" The human approves with a scope — narrow (this agent, this action, this exact resource), medium (this agent, this action, any resource of this type), or broad (this agent, any action on this resource type). Temper generates the Cedar policy and hot-loads it. The agent retries and succeeds.

Over time, the policy set converges on what the agent actually needs. The human is not writing policies from scratch — they are reviewing and approving agent requests as they arise. The governance experience is reactive, not proactive: you don't have to anticipate what your agent will need; you respond when it asks.

Eventually, the human may delegate some governance to a security agent or audit agent that reviews requests against organizational rules. The approval flow is the same; the approver changes.

### Everything Is Recorded

Every action an agent takes through Temper is a state transition. Every state transition is persisted. Every transition carries the agent's identity, the action, the before/after state, whether authorization succeeded or was denied, and the Cedar policy that governed the decision.

This gives you three things:

1. **Audit trail.** What did this agent do? When? What was the state before and after? Was it authorized? By whom? Every question has an answer in the trajectory log.

2. **Agent self-awareness.** The agent can query Temper to know where it is. What's the current state of the expense report? Which steps are complete? What's blocked? The state machine is the agent's memory of its own work.

3. **Cross-agent visibility.** If multiple agents share a Temper instance, they can see each other's state. One agent's completed step unblocks another agent's next action. This is not a messaging system — it's shared verified state.

### External Access Is Governed

When an agent needs to call an outside system — a payment API, a notification service, a database — it does so through an integration declared in the IOA spec. The integration is part of the verified plan. Cedar policies govern which external calls are permitted for which agents.

The execution model: the agent submits a spec with an `[[integration]]` section declaring the external call. The WASM module that implements the call runs in a sandboxed environment. The Cedar policy evaluates whether this agent, performing this action, is allowed to access this external resource. If denied, the pending decision flow surfaces it to the human.

In the vision, WASM integration modules can be reviewed in real time by a security agent, or they can be formally verified — the same way the state machine specs are verified today. An agent's external access is not just authorized; it is inspectable and provable.

### The Interface Is a REPL

Agents interact with Temper through a sandboxed code interface — not JSON tool-calling, not a chat interface. Think Symbolica's Agentica or Cloudflare's Code Mode: the agent writes code against a typed API surface, and that code executes in a sandbox whose only access to the outside world is through Temper.

Through this REPL, an agent can:

- **Register specs.** Generate and submit IOA specifications that define new state machines, new actions, new integrations.
- **Write integrations.** Submit WASM modules that implement external API calls, governed by Cedar policies.
- **Call existing APIs.** Operate on entities that already exist — create orders, transition states, query history.
- **Query its own state.** Read trajectory logs, check entity status, discover available actions through OData `$metadata`.

The REPL is the only tool the agent is given for state-changing operations. It cannot call external APIs directly. It cannot write to databases. It cannot execute arbitrary code outside the sandbox. Everything that mutates state goes through Temper, which means everything is governed and recorded.

## Two Use Cases, One Architecture

### Personal Assistant / Employee Agent

A human has one or more agents — a personal assistant, a coding agent, a research agent. Each agent operates through Temper. The human sets Cedar policies governing what each agent can do.

The agent is both the developer and the user of its own specs. When the personal assistant needs to manage a project, it generates a project management spec (issues, states, transitions, invariants), registers it with Temper, and operates through it. When the coding agent needs to coordinate a deployment, it generates a deployment spec with integration hooks to CI/CD systems, governed by policies that the human approved.

Multiple agents belonging to the same human may share one Temper instance and one database if they need shared state — or they may each run their own instance. That depends on needs. Sharing is optional; governance is not.

### Enterprise API

A developer (human or agent) builds an application through Temper — entity types, state machines, authorization policies, integrations. Other agents, belonging to other users, consume that application through the OData API. This is the traditional two-sided model: one side builds, the other side uses.

The same governance applies. Consuming agents are authorized by Cedar policies. Their actions are recorded. Denied requests surface to the application owner.

### The Convergence

These are not separate products. They are the same architecture serving different deployment shapes. The spec-first, verify-before-execute, govern-with-Cedar, record-everything model is the same in both cases. What changes is who builds the specs and who sets the policies.

## What Exists Today

The foundation is built:

- **Cedar authorization** with three-level evaluation (entity actions, WASM host functions, secret pre-filtering) and atomic policy hot-reload.
- **Agent identity** threaded through the full dispatch chain (`X-Agent-Id`, `X-Session-Id`).
- **Pending decision flow.** Default-deny posture. Denials surface to the human with approve/deny UX and Cedar policy auto-generation at three scopes (narrow, medium, broad).
- **Trajectory logging** with per-agent audit trails — full action history, denial tracking, entity type breakdowns.
- **Observe dashboard** with decisions page (live SSE updates, approval workflow) and agents page (action counts, denial rates, timelines).
- **Four-level verification cascade** for all specs before deployment.
- **OData self-describing API** with agent hints, success rates, and `$metadata` discovery.
- **Integration engine** with outbox pattern, WASM sandbox, and credentials vault.
- **Idempotency** for agent requests (agents making duplicate calls is safe).
- **Spec submission API.** `POST /api/specs/load-inline` accepts IOA TOML and CSDL as JSON, runs the full verification cascade, and deploys — an agent can generate specs and submit them through this endpoint today.
- **Deploy pipeline.** `DeployPipeline::verify_and_deploy()` orchestrates verification, registration, and broadcast for programmatic spec deployment.
- **Evolution agents.** Claude-powered observation and analysis agents that formalize unmet intents into structured problem statements and propose spec changes.

## What's Next

The vision, not yet built:

- **REPL interface.** A sandboxed code execution environment (Agentica/Code Mode style) as the primary agent interface to Temper. Agents write code against a typed API; the sandbox mediates all external access through Temper.
- **Security review agents.** Delegated governance where a security agent reviews WASM modules and integration requests on behalf of the human.
- **Formal verification of integrations.** Extending the verification cascade to cover WASM integration modules, not just state machine specs.
- **Cross-agent coordination primitives.** Explicit mechanisms for agents to observe and coordinate through shared Temper state.

## The Core Bet

The bet is that agents need governance the same way processes need an operating system. An OS doesn't tell a program what to do — it mediates the program's access to shared resources (memory, disk, network) and ensures programs can't corrupt each other's state. Temper does the same for agents: it mediates access to state and external systems, ensures actions are authorized, and maintains a verifiable record of everything that happened.

The alternative — agents with direct access to everything, governed by prompt engineering and hope — works until it doesn't. The question is not whether agents need governance. The question is whether governance can be formal, verified, and transparent rather than ad hoc.

That is what Temper is for.
