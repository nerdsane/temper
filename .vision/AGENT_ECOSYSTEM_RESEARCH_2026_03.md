# Agent Ecosystem Research & Temper Vision Exploration

**Date**: March 25-26, 2026
**Context**: Deep exploration of where Temper fits in the emerging agent ecosystem, conducted from first principles with deliberate detachment from Temper's current architectural decisions.

---

## Table of Contents

1. [The Landscape Today](#the-landscape-today)
2. [Projects Reviewed](#projects-reviewed)
3. [Key Research Findings](#key-research-findings)
4. [First-Principles Assessment of Temper's Bets](#first-principles-assessment-of-tempers-bets)
5. [The Behavior-First Vision](#the-behavior-first-vision)
6. [The Reusability Layers](#the-reusability-layers)
7. [The "What's the Reusable Unit?" Question](#whats-the-reusable-unit)
8. [The Observation Position Problem](#the-observation-position-problem)
9. [Open Questions](#open-questions)
10. [Strategic Implications](#strategic-implications)

---

## The Landscape Today

### Execution Is Commoditizing

Sandboxed execution for agents is a solved/solving problem with 5+ providers racing to the bottom:

- **E2B**: Firecracker microVMs, ~half the Fortune 500, the incumbent
- **Daytona**: Pivoted to agent infra, $24M Series A (Feb 2026), sub-90ms cold starts, $1M ARR in <3 months
- **Cloudflare Dynamic Workers**: V8 isolates, 100x faster than containers, $0.002/worker/day, millions of agents per-user
- **Modal**: gVisor-based, GPU-first
- **Others**: Northflank, Koyeb, Vercel — all shipping sandbox features

**Signal**: Stateful, long-running agents with snapshot/fork/resume are the new frontier. Stateless sandboxes are table stakes.

### MCP Is the Integration Standard

- All major AI providers support MCP (OpenAI, Anthropic, Google, Amazon, Microsoft)
- 34,700+ dependent projects on the TypeScript SDK
- `.well-known/mcp/server-card.json` (SEP-1649) will make MCP servers discoverable network services
- Streamable HTTP unlocked remote MCP servers

### A2A Is Emerging for Agent-to-Agent

- Google's Agent2Agent Protocol, 50+ partners (Atlassian, Salesforce, SAP, PayPal)
- Agent Cards (JSON) advertise capabilities, registry endpoints for discovery
- v0.3 added gRPC support and signed security cards
- MCP (agent-to-tool) + A2A (agent-to-agent) becoming complementary standards

### Agent Identity Is the Biggest Unsolved Problem

- Only 22% of teams treat agents as independent identities
- Non-human identities outnumber humans 50:1
- 88% of organizations have confirmed or suspected security incidents involving agents
- Only 14.4% have full security approval before agents go to production
- NIST published concept paper on agent identity (Feb 2026)
- Gartner predicts 40%+ of agentic AI projects fail by 2027 due to insufficient risk controls

### Governance Is Immature

- Only 20% of organizations have mature governance models
- 82% of executives think they're protected; 14% actually are
- The model is NOT the bottleneck — integration, auth, reliability, and governance are

### AI-Generated Code Quality Crisis

- 45% of AI-generated code contains security vulnerabilities (Veracode)
- 80% of AI-generated applications have exploitable vulnerabilities (Stanford)
- 1.7x more issues per PR, 4x more code cloning, 8x more excessive I/O
- Silent failures: LLMs generate code that runs but removes safety checks or fakes output
- Lovable: 100,000 new projects/day. Neither Lovable nor Bolt publishes survival rates.

### Framework Consolidation

- Vendor SDKs (OpenAI Agents SDK, Claude Agent SDK, Google ADK) eating framework market share for new projects
- LangGraph, CrewAI specializing in complex orchestration
- Lance Martin (LangChain) rebuilt his agent system twice in 18 months — model improvements made scaffolding a bottleneck

---

## Projects Reviewed

### 1. Cloudflare Dynamic Workers

**URL**: https://blog.cloudflare.com/dynamic-workers/
**What**: V8 isolate-based sandboxed execution for AI-generated code at global scale.
**Key features**: Sub-ms cold starts, no concurrency limits, credential injection via globalOutbound, TypeScript API exposure (81% token reduction vs tool schemas), battle-hardened V8 security.
**Relevance**: Solves execution. Does NOT solve correctness, governance, or trust. An agent can generate a function that transfers money to the wrong account — it just does so in an isolate.

### 2. Executor (RhysSullivan/executor)

**URL**: https://github.com/RhysSullivan/executor
**What**: Local-first execution environment for AI agents. Control plane that mediates tool interactions.
**Key features**: Semantic tool discovery (`tools.discover({ query: "github issues" })`), managed OAuth/credentials, pause/resume for human-in-the-loop, MCP bridge, multiple sandbox runtimes (QuickJS, SES, Deno).
**Architecture**: CLI + HTTP server + SDK + Web UI, all on localhost. Sources: MCP servers, OpenAPI REST, GraphQL.
**Relevance**: Solves tool mediation and credential management. Positions itself as "the final form of tool calling." Does not address correctness, trust, or evolution.

### 3. Agent Auth Protocol

**URL**: https://agent-auth-protocol.com
**Repo**: https://github.com/better-auth/agent-auth (13 stars, created Feb 20, 2026)
**What**: Open-source auth/authz standard for AI agents.
**Key features**: Per-agent Ed25519 cryptographic identity, capability-based authorization with human approval, `/.well-known/agent-configuration` discovery, directory at agent-auth.directory.
**Flow**: Agent discovers service → registers with public key → requests capabilities → human approves → agent executes with signed JWT.
**Ships**: Better Auth server plugin, client SDK, CLI + MCP server, OpenAPI/MCP adapters.
**What it has**: Identity, authorization, discovery.
**What it lacks**: Observation (no record of what agent did), trust gradient (capabilities are binary granted/not), evolution, verification.
**Relevance**: Solves the wire protocol for agent identity and authorization. The "lock on the door, not the operating system inside the house." Complementary to a trust/observation layer.

### 4. OpenSpace (HKUDS/OpenSpace)

**URL**: https://github.com/HKUDS/OpenSpace (60 stars, created Mar 24, 2026)
**What**: Self-evolving skill engine that plugs into any agent via MCP.
**Key claims**: 4.2x higher income on professional tasks, 46% fewer tokens on warm runs, 165 skills autonomously evolved from 50 tasks.

**How it actually works**:
- Exposes 4 MCP tools: `execute_task`, `search_skills`, `fix_skill`, `upload_skill`
- When an agent calls `execute_task`, OpenSpace runs its **own internal GroundingAgent** (makes its own LLM calls via LiteLLM using inherited API keys)
- Records everything its own agent does: `traj.jsonl` (tool calls), `agent_actions.jsonl` (decisions), `conversations.jsonl` (LLM interactions)
- Post-execution: LLM-driven analyzer reviews recording, produces evolution suggestions
- Evolver runs another LLM agent loop to generate FIX/DERIVED/CAPTURED skill changes
- Skills stored in SQLite with full version DAG, lineage, quality metrics

**Three evolution triggers**:
1. Post-execution analysis (after every task)
2. Tool degradation (when success rates drop, batch-evolve dependent skills)
3. Metric monitor (periodic scan of skill health metrics)

**Cloud sharing**: REST API at open-space.cloud. Upload/download SKILL.md files + metadata. Hybrid BM25 + embedding search. **No telemetry or trajectory sharing — only skill artifacts.**

**Critical limitation**: It does NOT observe the host agent (Claude Code, etc.). It runs its own proxy agent. When Claude Code delegates via `execute_task`, OpenSpace's internal agent does the work. The `search_skills` path (agent discovers skill and uses it directly) produces no observation, no analysis, no evolution. **Evolution only happens for tasks delegated through execute_task.**

**The reusable unit**: SKILL.md — a markdown file with `name` and `description` frontmatter and free-form body. Natural language instructions. The most evolved skill went through 13 versions, all markdown. Most evolved skills focus on error recovery and tool reliability, not domain knowledge.

### 5. iron-sensor (ironsh/iron-sensor)

**URL**: https://github.com/ironsh/iron-sensor (25 stars, created Mar 23, 2026)
**What**: eBPF-based behavioral monitor for AI coding agents. Sits in the Linux kernel and watches what agents actually do.
**How**: 6 kernel tracepoints (process exec/fork/exit, file open, permission changes). Detects agents by executable name, propagates tracking to entire process subtree. Emits structured NDJSON events classified by severity.
**Detects**: Privilege escalation, SSH key access, cron writes, systemd modifications, network tool usage, Docker socket access, sensitive file access.
**Agent detection**: Claude Code (`argv[0]` = `claude`), OpenClaw (`openclaw-gateway`), Codex (python3 + codex in argv).
**Key insight**: Proves you can observe agents **without being inside them** — from the kernel. Agent doesn't know it's being watched. Near-zero overhead.
**Limitation**: Observes syscalls (low-level security), not semantic behavior (high-level capabilities). Sees "agent read /etc/shadow" not "agent successfully built a dashboard."
**Relevance**: The observation layer at the lowest altitude. Built for security, not evolution. But the architectural pattern (observe from below, classify, emit structured events) is exactly what a trust layer needs.

### 6. Pydantic AI Capabilities (v1.71.0)

**URL**: https://github.com/pydantic/pydantic-ai/releases/tag/v1.71.0 (released Mar 24, 2026)
**What**: Composable, reusable units of agent behavior that bundle tools, lifecycle hooks, instructions, and model settings into a single class.

**Architecture**:
- Subclass `AbstractCapability[T]`, override needed methods
- Configuration: `get_toolset()`, `get_builtin_tools()`, `get_instructions()`, `get_model_settings()`
- Lifecycle hooks at every level: run, node, model request, tool validation, tool execution
- Before/after/wrap/error variants at each level
- `wrap_*` hooks give middleware-style control (intercept, transform, retry, skip)

**Composability**: Multiple capabilities compose automatically. Before hooks in order, after hooks in reverse, wrap hooks nest as middleware.

**Provider-adaptive tools**: `WebSearch`, `WebFetch`, `MCP` auto-detect native support, fall back to local.

**Spec serialization**: Capabilities can be defined in YAML/JSON, loaded via `Agent.from_file()`.

**Key insight**: The hook system IS an observation layer — from INSIDE the agent. A capability that wraps tool execution and model requests sees exactly what OpenSpace wants to see (the agent's actual behavior) but from within the agent's own execution loop, not a proxy.

**You could build an OpenSpace-style evolution engine as a Pydantic AI Capability** — wrap every tool call, hook into model requests, analyze post-run, evolve skills — all without delegating to a separate agent.

---

## Key Research Findings

### The Bitter Lesson Applied to Agents

- **Lance Martin (LangChain)**: Rebuilt his agent system twice in 18 months. Model improvements made scaffolding a bottleneck. The person building a top agent framework is warning that frameworks have a half-life of months.
- **Hyung Won Chung (OpenAI)**: "The community loves adding structures but there is much less focus on removing them." You add structure for current capability level, then must remove it when capabilities improve.
- **Every agent framework is a bet against model improvement.** The more opinionated, the more you'll tear out.
- **Historical pattern**: Every platform shift (web, mobile, cloud) over-prescribed structure at 2-3 years. Walled gardens, native-only apps, lift-and-shift. Winners provided primitives, not solutions.

### Emergent Behavior in Multi-Agent Systems

- Decentralized multi-agent systems consistently outperform both single agents AND scripted cooperative systems
- Spontaneous leadership, norm formation, role specialization emerge without programming
- But so do deception, collusion, and moral drift
- Heavy orchestration may suppress the emergent intelligence that makes multi-agent systems valuable
- The platform's job: provide substrate (communication, memory, identity), not prescribe coordination

### Self-Improving Systems

- **Live-SWE-agent**: Starts with minimal tools, rewrites its own scaffolding at runtime. 79.2% on SWE-bench. Zero offline training cost.
- **Darwin Godel Machine**: Autonomously improved from 20% to 50% on SWE-bench by modifying its own code.
- If agents can rewrite their own scaffolding, building elaborate frameworks is doubly futile.

### Tools Are Dissolving

- Cloudflare Code Mode: 81% fewer tokens when agents write TypeScript against SDKs vs. structured tool calls
- LLMs prefer typed interfaces over tool-calling protocols
- The tool/code/API boundary is dissolving — for LLMs, it's all text
- MCP may be the last protocol standard in its current form

### Cross-Agent Learning Barely Exists

- Letta: shared memory blocks between agents
- MemOS: multi-agent memory sharing (launched March 2026)
- CLAUDE.md files: dominant form of "shared agent knowledge" in practice
- True cross-agent learning (agent A's experience autonomously improves agent B) is essentially zero outside research

### The Software Distribution Arc

Binaries → source → packages → containers → APIs → functions → **???**

Each step increased the ratio of intent to implementation. The next unit might be an **intent**, not a thing. If so, everything we're building around agent packaging may be as temporary as 1990s portal strategies.

---

## First-Principles Assessment of Temper's Bets

### What Survives

| Decision | Why it survives |
|----------|----------------|
| **Cedar authorization** | Default-deny, agent identity, scoped permissions. The market is screaming for this (only 22% have agent identity). Doesn't constrain intelligence, constrains damage. Durable infrastructure. |
| **Event sourcing / audit trail** | Recording what happened is a permanent need. Raw material for debugging, compliance, learning, evolution. |
| **Evolution from usage (GEPA)** | The single most differentiated thing. Aligned with the bitter lesson — leverages observation and scale, not upfront human knowledge. Nobody else is doing "capabilities improve through use." |
| **Human approval gate** | Humans govern the boundaries. Approve trust escalation, contract changes, capability expansion. This principle is permanent. |
| **Self-describing APIs** | Agents need to discover valid actions. The principle survives regardless of specific protocol. |

### What's Questioned

| Decision | The concern |
|----------|------------|
| **State machines as primary abstraction** | The governance contract idea is sound, but named states + enumerated transitions may be too rigid. Agent behavior doesn't always decompose into finite states. Could be one possible formalism among many, not the required one. |
| **Specs as mandatory starting point** | Behavior-first, spec-later may be more natural. The bitter lesson suggests: let agents act, then extract structure from successful behavior, rather than requiring declaration upfront. |
| **Formal verification as universal gate** | Powerful tool, wrong as universal requirement. Should be available for high-stakes transitions, not mandated for everything. Trust gradient > binary gate. |
| **OData as the API layer** | Too specific. Agents may prefer function calling, TypeScript SDKs, or MCP. The principle (self-describing, discoverable) is good; the specific protocol is an implementation detail. |
| **Temper owns the runtime** | Execution is commoditizing. Temper's value is governance, not hosting. Could govern any runtime rather than being one. |

### What Might Need to Change

The overall direction: from **spec → verify → deploy → operate** to **operate → observe → extract → verify → govern**.

Verification moves from the entry gate to the crystallization boundary. It hardens what works rather than permitting what might work.

---

## The Behavior-First Vision

### The Principle

**Behavior → spec extraction → governance**, rather than spec → verification → behavior.

### The Lifecycle

1. **Agent acts freely** in any sandbox (Cloudflare, E2B, wherever)
2. **System observes** — captures behavior, traces, outcomes
3. **Patterns are extracted** — "this agent reliably does X when given Y, maintaining invariant Z"
4. **Contract crystallizes** — not necessarily a state machine; could be invariants, capabilities, trust level
5. **Other agents discover and use it** — by intent, not by name
6. **Usage feeds back** — the capability improves, its trust level changes, its contract evolves
7. **Human governs the boundaries** — Cedar policies, approval gates for trust escalation

### What Changes

| Today's Temper | Behavior-First Temper |
|----------------|----------------------|
| Spec first, then verify, then deploy | Act first, observe, extract contract, then verify |
| State machines are the abstraction | Contracts are flexible — invariants, capabilities, trust levels |
| Temper owns the runtime | Temper governs any runtime |
| OData is the API | MCP + A2A are the interfaces |
| Verification is a gate | Verification hardens what works |
| Single-node, self-contained | Governance layer over distributed execution |

### What Stays

- Cedar authorization (beating heart)
- Event sourcing / audit trail (permanent need)
- GEPA evolution (strongest bet — becomes even more powerful as the mechanism that creates structure from behavior)
- Verification cascade (still exists, runs at crystallization boundary)
- Human approval gate (governing principle)

### Verification's New Role

Verification becomes a tool for **hardening what works**, not a gate for **permitting what might work**.

An agent builds a task management capability — maybe observation and trust scoring is enough. An agent builds a capability that handles financial transactions — now verification is invoked before trust can escalate beyond a threshold. The stakes determine the rigor.

---

## The Reusability Layers

### Layer 0: Raw Execution
Agent runs code in a sandbox. Ephemeral. Not reusable. Commodity (Cloudflare, E2B).

### Layer 1: Running Capabilities
An agent built something that stays running and has an interface. Discoverable via MCP .well-known. But millions of these will exist — how does an agent know which is good, safe, or correct? MCP tells you something exists. It doesn't tell you whether to trust it.

### Layer 2: Observed Behavior / Trust Record
Sits on top of discovery. Not "this capability exists" but "this capability has processed 10,000 requests, maintained these invariants, failed in these ways, earned this trust level, and a human approved its governance boundary." Turns a directory into a reputation system. **Without this, agent-to-agent discovery is the early web without PageRank.**

### Layer 3: Extracted Patterns
Across many capabilities in Layer 1, observed by Layer 2, patterns emerge. "Capabilities that manage tasks converge on these flows." "Capabilities that handle payments maintain these invariants." Abstract, portable, reusable across domains. An agent starting from scratch uses a pattern as starting point.

### Layer 4: Collective Intelligence
The flywheel. Agents act → observation records accumulate → trust is earned → capabilities become discoverable → other agents use them → more observation → patterns extracted → new agents start from better patterns → ecosystem gets smarter.

### Where Value Lives

MCP and A2A own discovery protocol. Cloudflare/E2B own execution. Those are commodities or open standards.

**Value is in Layers 2 and 3** — the trust record and pattern extraction. Because:
- Discovery without trust is useless (million capabilities, can't tell which are reliable)
- Capabilities without observation can't improve (ecosystem stays static)
- Individual capabilities without pattern extraction don't compound (each agent starts from scratch)

---

## What's the Reusable Unit?

Different projects give different answers:

| Project | Reusable Unit | Format |
|---------|--------------|--------|
| GitHub/npm | Code packages | Source code + manifest |
| Temper (current) | Verified specs | IOA TOML + CSDL + Cedar |
| OpenSpace | Skills | SKILL.md (natural language markdown) |
| Pydantic AI | Capabilities | Python classes with defined interface |
| MCP ecosystem | Tool servers | Protocol endpoints |
| Cloudflare | Functions | JavaScript in V8 isolates |

**Key insight from OpenSpace**: The reusable unit might just be **text** — natural language instructions. The simplest, most general, most LLM-native representation. OpenSpace's most evolved skill went through 13 versions, all markdown. Most skills focus on error recovery patterns, not domain knowledge.

**Key insight from Pydantic AI**: The reusable unit might be **composable behavior with hooks** — richer than text, with defined interfaces for observation and composition.

**Key insight from the vision discussion**: The reusable unit might be a **running capability** — not a static artifact but a living thing that other agents interact with. The registry and the runtime are the same. You don't download and install; you discover and use.

**The bitter lesson suggests**: The most general representation wins. That might be text (LLM-native), running capabilities (already operational), or something we haven't identified yet. Formal specs are powerful but may be too rigid as a universal unit.

**Unresolved**: Whether the reusable unit is one thing or whether different layers have different natural units (text for patterns, running services for capabilities, signed contracts for trust).

---

## The Observation Position Problem

A critical architectural question: **where do you sit to observe agent behavior?**

| Position | What you see | Who's doing it |
|----------|-------------|---------------|
| **Inside the agent** | Everything: reasoning, tool calls, results, failures | Pydantic AI Capabilities (hooks at every level) |
| **Between agent and tools** | Tool calls, args, results | Executor, MCP servers |
| **Separate agent (proxy)** | Only what the proxy does, not the host agent | OpenSpace |
| **Below (kernel)** | Syscalls: files, processes, network | iron-sensor (eBPF) |
| **At the gate** | Auth decisions, capability grants | Agent Auth Protocol |
| **Above (platform)** | API calls, state transitions | Temper (current) |

**The gap**: Nobody is observing the actual host agent's semantic behavior (not syscalls, not just tool calls, but what the agent was trying to do and whether it succeeded) and using that to evolve reusable artifacts.

- OpenSpace sidesteps by running its own agent
- iron-sensor sees syscalls, not semantics
- Pydantic AI created the hookpoints but nobody's built the evolution capability yet
- Temper observes what flows through its own API but doesn't observe agents operating elsewhere

**The opportunity**: The "inside the agent" position (Pydantic AI-style hooks) combined with the evolution loop (GEPA-style) would be genuinely novel. Observe the actual agent, extract patterns from its actual behavior, evolve reusable artifacts. This doesn't exist yet.

**The challenge**: Being inside the agent requires framework adoption (Pydantic AI, or equivalent hooks in other frameworks). Being outside (like iron-sensor) is more universal but less semantic. There may be a middle ground: observing artifacts the agent naturally produces (git commits, tool call logs, CLAUDE.md updates) without being inside the execution loop.

---

## Open Questions

### Vision & Architecture

1. **Is the reusable unit one thing or many?** Text at the pattern layer, running capabilities at the service layer, formal contracts at the trust layer? Or does one representation win across all layers?

2. **Can you get semantic observation without being inside the agent?** Agents leave trails (git commits, file changes, tool call logs). Is that enough, or do you need the Pydantic AI-style hooks?

3. **Does formal verification survive the behavior-first model?** If so, where does it sit? At crystallization (extracting contracts from behavior)? At trust escalation (high-stakes actions only)? Or does runtime monitoring + trust scoring replace it entirely?

4. **What happens to state machines?** Are they one possible contract format among many? Do they emerge naturally from behavior observation? Or are they an unnecessary formalism that agents will route around?

5. **How does the trust gradient work mechanically?** What's the scoring model? Is it per-capability, per-agent, per-action? How does trust transfer (if I trust capability A and it composes with capability B...)?

### Market & Strategy

6. **Is Temper the governance layer, the trust layer, or the evolution layer?** These are different products. Governance (Cedar) is infrastructure. Trust (observation + scoring) is a platform. Evolution (GEPA) is intelligence. Which one is the wedge?

7. **Should Temper own a runtime at all?** If execution is commodity, should Temper be a governance sidecar that works with any runtime? Or does owning the runtime give you observation advantages?

8. **How does Temper relate to Agent Auth Protocol?** Complementary (AAP does wire protocol, Temper does trust)? Competitive? Should Temper adopt AAP for identity and focus on what sits above?

9. **OpenSpace validates the skill evolution thesis but with a simpler mechanism (LLM-driven analysis + markdown skills). Is Temper's formal methods approach overkill?** Or is the security/trust gap in OpenSpace the exact opening?

10. **Who is the customer for the first version?** Teams deploying agents at scale who've already been burned? Enterprise compliance requirements? Or the broader developer community?

### Technical

11. **Can GEPA work as a contract extractor (behavior → formal contract) rather than just a spec optimizer?** This would be the key technical pivot.

12. **What does "trust score" look like concretely?** What metrics? What thresholds? How is it computed from observation data?

13. **How do you handle the cold start problem?** A new capability has no observation history, thus no trust. How does it bootstrap?

14. **What's the relationship between MCP .well-known discovery and Temper's trust layer?** Does Temper extend the server card with trust metadata?

---

## Strategic Implications

### The One-Sentence Positioning Options

- **Current**: "Temper is a verified operating layer for governed applications."
- **Governance focus**: "Temper is the governance layer for the agent economy. Agents run anywhere. Temper makes them trustworthy."
- **Trust focus**: "MCP tells you a capability exists. Temper tells you whether to trust it."
- **Evolution focus**: "The only platform where agent capabilities improve through use."
- **Combined**: "Agents run anywhere. Temper watches what they do, earns trust from behavior, and makes the whole ecosystem smarter."

### What Only Temper Has (That Nobody Else Does)

1. **Formal verification cascade** — can prove specs correct (even if role changes to hardening, not gating)
2. **GEPA evolution engine** — end-to-end loop proven, nobody else has "capabilities improve through use"
3. **Cedar authorization** — complete default-deny with agent identity, decision approval UI
4. **Event-sourced audit trail** — full state transition history with agent attribution

### The Competitive Landscape Summary

| Layer | What | Who | Temper's position |
|-------|------|-----|-------------------|
| Execution | Sandboxed compute | Cloudflare, E2B, Daytona, Modal | Don't compete. Use as substrate. |
| Tool mediation | Discovery + credentials | Executor, Composio, Toolhouse | Adjacent. Not core. |
| Identity protocol | Auth + authz wire format | Agent Auth Protocol, Permit.io | Adopt or integrate. Not core. |
| Integration standard | Tool access protocol | MCP | Participate. Not own. |
| Agent-to-agent | Discovery + delegation | Google A2A | Participate. Not own. |
| Skill evolution | Behavior → better skills | OpenSpace | Overlap in vision. Different mechanism. OpenSpace lacks trust/security. |
| Security monitoring | Detect malicious behavior | iron-sensor | Complementary. Different altitude. |
| Agent framework | Composable behavior | Pydantic AI, LangGraph, CrewAI | Integrate with. Not compete. |
| **Trust + governance** | **Observation → trust → governance** | **Nobody** | **This is the gap.** |
| **Verified evolution** | **Formally verified improvement** | **Nobody** | **Temper's unique combination.** |

### The Core Bet

The agent ecosystem is building execution (Cloudflare), tools (MCP/Executor), discovery (A2A/.well-known), identity (Agent Auth), and even skill evolution (OpenSpace).

Nobody is building the **trust layer** — the thing that sits between discovery and use and answers: "should you trust this?" based on observed behavior, not self-reported claims. And nobody is combining trust with **verified evolution** — capabilities that improve through use AND can prove their improvements are correct.

That's the gap. Whether it's a gap the market is ready to pay for today is the validation question from the beginning of this conversation.

---

### 7. Alita (Princeton — CharlesQ9/Alita)

**URL**: https://github.com/CharlesQ9/Alita (882 stars)
**Paper**: [arXiv:2505.20286](https://arxiv.org/abs/2505.20286) (May 2025)
**What**: Generalist agent that autonomously creates MCP servers when it encounters tasks it can't handle. #1 on GAIA benchmark (75.15% pass@1), beating OpenAI Deep Research and Manus.

**How it works**:
1. Agent encounters a task requiring a capability it doesn't have
2. Manager Agent brainstorms what tool is needed, searches GitHub for relevant code
3. ScriptGeneratingTool creates a Python implementation with environment setup
4. CodeRunningTool tests in isolated environment with iterative refinement
5. Working tool is packaged as an MCP server and stored in "MCP Box"
6. Future tasks reuse existing MCP servers; library grows over time

**Key results**:
- 75.15% pass@1 on GAIA validation (87.27% pass@3)
- MCP creation contributed ~15% increase in pass@1 on GAIA test
- When Alita's MCPs given to GPT-4o-mini agents: hardest-task accuracy tripled (3.85% → 11.54%)
- Follow-up Alita-G achieved 83.03% pass@1 while reducing tokens by 15%

**What it validates**: Running MCP servers as the reusable unit. Behavior-first tool creation (encounter problem → build solution → solution becomes reusable). Agent distillation via tool sharing.
**What it lacks**: No verification of generated tools. No governance. No trust scoring. No feedback from consumers. "Ugly codes" by their own admission. One-directional sharing (creator → consumer), no collective improvement.

### 8. ClawTeam (HKUDS/ClawTeam)

**URL**: https://github.com/HKUDS/ClawTeam (3,721 stars in 9 days, created Mar 17, 2026)
**What**: Multi-agent swarm orchestrator. Leader agent spawns worker agents, each with own git worktree and tmux window. Agents coordinate via CLI commands auto-injected into prompts.

**How it works**:
- All state is JSON files in `~/.clawteam/`. No database, no server.
- Leader calls `clawteam spawn` to create workers with dedicated git branches
- Workers check tasks, update status, and message each other via CLI commands
- Agent-agnostic: Claude Code, Codex, OpenClaw, any CLI agent
- Task dependencies with auto-unblocking on completion
- TOML templates for team archetypes (hedge fund, research team, etc.)

**Key demo**: 8 agents across 8 H100 GPUs, 2,430 autonomous ML experiments, 6.4% val_bpb improvement, zero human intervention.

**Same lab as OpenSpace** (HKUDS). Building the full stack: nanobot (agent) + OpenSpace (skill evolution) + ClawTeam (swarm orchestration).

**What it validates**: Emergent coordination works. Agents self-organize effectively with minimal structure. Massive developer interest (3,700 stars in days).
**What it lacks**: Zero governance. Default `skip_permissions: true`. No audit trail, no event sourcing, no trust model. Auth/permissions/audit listed as v1.0 (last phase of roadmap).

### 520 Tool Misuse Incidents

**Source**: 2026 agentic AI security threat reports (Stellar Cyber, Lasso Security, OWASP)
**What**: 520 reported incidents of tool misuse and privilege escalation — the most common category of agent security incidents in 2026.

**Pattern**: The confused deputy problem. Agents have broad legitimate permissions (CRMs, code repos, cloud infra, financial systems). Attackers craft inputs that trick agents into using those permissions for unauthorized purposes.

**Real example**: Financial reconciliation agent tricked into exporting "all customer records matching pattern X" where X was a regex matching every record. 45,000 customer records exfiltrated. Agent had legitimate export permissions.

**OWASP Top 10 for Agentic Applications (2026)** lists Tool Misuse and Exploitation (ASI02) as a top risk.

**Relevance**: This is the measured cost of building execution and capability without governance. ClawTeam spawns with `skip_permissions: true`. Alita auto-generates MCP servers unverified. OpenSpace shares skills without trust scoring. The 520 incidents are what happens.

---

## Updated Reusable Unit Comparison

| Project | Reusable Unit | Format | Verified? | Governed? | Evolves? |
|---------|--------------|--------|-----------|-----------|----------|
| GitHub/npm | Code packages | Source + manifest | No | No | Manual |
| Temper | Verified specs | IOA TOML + CSDL + Cedar | Yes (4 levels) | Yes (Cedar) | Yes (GEPA) |
| OpenSpace | Skills | SKILL.md (markdown) | No | No | Yes (LLM-driven) |
| Pydantic AI | Capabilities | Python classes | No | No | No |
| Alita | MCP servers | Python + MCP wrapper | Tested only | No | Accumulates |
| ClawTeam | Team templates | TOML | No | No | No |
| MCP ecosystem | Tool servers | Protocol endpoints | No | No | No |

**Alita's answer is the strongest signal yet**: the reusable unit for agents is a running, callable capability with a standardized interface (MCP server). Not text, not specs, not code — a living tool. And it demonstrably works (tripled weak agents' performance on hard tasks).

**But the governance gap is measured**: 520 incidents. The tools work. They're just not verified, governed, or trusted. That's the opening.

---

## Appendix: Sources

### Blog Posts & Documentation
- [Cloudflare Dynamic Workers](https://blog.cloudflare.com/dynamic-workers/)
- [Cloudflare Code Mode](https://blog.cloudflare.com/code-mode-mcp/) — 81% token reduction
- [Lance Martin - Learning the Bitter Lesson](https://rlancemartin.github.io/2025/07/30/bitter_lesson/)
- [Citrix - The Bitter Lesson of Workplace AI](https://www.citrix.com/blogs/2025/09/17/the-bitter-lesson-of-workplace-ai-stop-engineering-start-enabling)
- [2026 MCP Roadmap](http://blog.modelcontextprotocol.io/posts/2026-mcp-roadmap/)
- [Pydantic AI Capabilities docs](https://ai.pydantic.dev/capabilities/)

### Research & Reports
- [CodeRabbit: AI code quality](https://www.coderabbit.ai/blog/state-of-ai-vs-human-code-generation-report) — 1.7x more issues
- [IEEE Spectrum: AI Coding Degrades](https://spectrum.ieee.org/ai-coding-degrades) — silent failures
- [LangChain State of Agent Engineering](https://www.langchain.com/state-of-agent-engineering) — 1,300+ respondents
- [Gravitee: AI Agent Security 2026](https://www.gravitee.io/blog/state-of-ai-agent-security-2026-report-when-adoption-outpaces-control)
- [MIT Technology Review: Guardrails to Governance](https://www.technologyreview.com/2026/02/04/1131014/from-guardrails-to-governance-a-ceos-guide-for-securing-agentic-systems/)
- [NIST: AI Agent Identity and Authorization](https://www.nccoe.nist.gov/sites/default/files/2026-02/accelerating-the-adoption-of-software-and-ai-agent-identity-and-authorization-concept-paper.pdf)
- [Emergent Intelligence in Multi-Agent Systems (TechRxiv)](https://www.techrxiv.org/users/992392/articles/1384935)
- [Live-SWE-agent](https://arxiv.org/abs/2511.13646) — self-improving agent, 79.2% SWE-bench
- [Darwin Godel Machine](https://sakana.ai/dgm/) — 20% → 50% autonomous improvement

### Repositories
- [Executor](https://github.com/RhysSullivan/executor) — local-first agent execution environment
- [Agent Auth Protocol](https://github.com/better-auth/agent-auth) — 13 stars, Feb 2026
- [OpenSpace](https://github.com/HKUDS/OpenSpace) — 60 stars, Mar 2026
- [iron-sensor](https://github.com/ironsh/iron-sensor) — 25 stars, Mar 2026
- [Pydantic AI](https://github.com/pydantic/pydantic-ai) — Capabilities in v1.71.0

### Industry Data Points
- Daytona: $24M Series A, $1M ARR in <3 months (Feb 2026)
- Lovable: 8M users, 100K projects/day, $200M ARR
- GitHub Copilot: 46% of code from active users, 20M cumulative users
- MCP: 34,700+ dependent projects
- A2A: 50+ enterprise partners
