# ADR-0038: temper-transport Crate — Platform-Agnostic Channel Transports

- Status: Proposed
- Date: 2026-03-25
- Deciders: Temper core maintainers
- Supersedes: ADR-0037 (channel-transports)
- Related:
  - ADR-0037: Original channel transport design (embedded in temper-server)
  - `os-apps/temper-channels/specs/` (Channel, AgentRoute, ChannelSession IOA specs)
  - `crates/temper-server/src/channels/` (current Discord transport, to be moved)

## Context

ADR-0037 introduced the Discord channel transport as a module inside `temper-server`. This was pragmatic for the first implementation but created problems:

1. **Transport code is tightly coupled to temper-server internals** — uses `ServerState`, `dispatch_tenant_action`, `event_tx` directly instead of the OData API
2. **Bypasses the Claw entity architecture** — the transport creates TemperAgent entities directly, tracks sessions in-memory with a `UserSession` struct, and persists to a JSON file hack instead of using the governed `Channel`, `AgentRoute`, and `ChannelSession` entities that already have IOA specs
3. **Not platform-agnostic** — Discord code is embedded in temper-server. Adding Slack/Teams would pollute the server with more platform-specific I/O
4. **One bot = one personality** — the hardcoded system prompt prevents multi-claw (multiple agent personalities) per tenant

The Claw architecture (Channel, AgentRoute, ChannelSession, AgentSoul) is fully specced with IOA state machines and WASM integrations. It just needs to be wired in.

## Decision

### Sub-Decision 1: New `temper-transport` crate

Create `crates/temper-transport/` as a workspace member. This crate:

- Defines the `TemperTransport` trait (connect, dispatch messages, deliver replies)
- Contains platform implementations (Discord first, Slack/Teams follow the pattern)
- Is a **Temper OData API client** — it imports `reqwest` and `tokio-tungstenite`, NOT `temper-server` or `temper-runtime`
- Communicates with Temper entirely through HTTP: `POST /tdata/Channels('{id}')/ReceiveMessage`, etc.

**Why this approach**: Transports are I/O adapters for external platforms. They don't need access to server internals — they need to dispatch actions and watch for events, which the OData API already supports. This separation makes transports independently deployable and testable.

### Sub-Decision 2: Wire through Claw entities

Instead of direct TemperAgent creation, the transport dispatches `Channel.ReceiveMessage`. The existing `route_message` WASM handles:

- Querying `AgentRoutes` to find the right `soul_id` and `agent_config`
- Creating or resuming `ChannelSession` entities for conversation continuity
- Creating `TemperAgent` entities with the route's soul and config
- Triggering `Channel.SendReply` when the agent completes

**Why this approach**: The Claw specs are already designed for this. Using them gives us governed sessions (Cedar auth), multi-claw per tenant (multiple AgentRoutes), and platform-agnostic routing (same WASM for Discord, Slack, etc.).

### Sub-Decision 3: AgentRoute + AgentSoul for multi-claw identity

Each claw is an `AgentRoute` with a `soul_id` pointing to an `AgentSoul` entity. The soul's `content_file_id` references a TemperFS file containing the claw's personality/instructions (equivalent to CLAUDE.md / soul.md). Multiple routes per channel enable multiple claw personalities per tenant.

**Why this approach**: Separates identity (Soul) from routing (Route) from session state (ChannelSession). A tenant can have many claws, each with different personalities, routed by channel, guild, or message pattern.

## Rollout Plan

1. **Phase 0** — Create `temper-transport` crate, move Discord code, convert to OData client
2. **Phase 1** — Wire Channel.ReceiveMessage → route_message WASM → TemperAgent → SendReply
3. **Phase 2** — Remove temper-server/src/channels/, verify multi-claw with AgentSoul

## Consequences

### Positive
- temper-server becomes a pure entity runtime (no platform I/O)
- New platforms = new file in temper-transport, same TemperTransport trait
- Multi-claw per tenant via AgentRoutes with different soul_ids
- Session persistence via ChannelSession entities (no more JSON file hacks)
- Transport is independently testable and eventually deployable as a separate binary

### Negative
- HTTP overhead for transport ↔ server communication (vs direct function calls)
- Two-hop latency for message dispatch (transport → OData → WASM → agent)

### Risks
- route_message WASM may need fixes for the Discord DM flow (thread_id = user_id for DMs)
- Channel.SendReply event subscription from an external client needs the observe API to support it

## Non-Goals

- Extracting temper-transport into a separate binary (future work, architecture supports it)
- Implementing Slack/Teams transports (Discord first, pattern established for future)
- Abstracting away platform-specific Gateway protocols (each platform has its own module)

## Alternatives Considered

1. **Keep in temper-server, just wire to Claw entities** — Less disruption, but keeps platform I/O in the server. Rejected because the clean separation is worth the migration effort.
2. **Separate binary immediately** — Cleanest architecture, but adds deployment complexity. Rejected in favor of a workspace crate that can be extracted later.
