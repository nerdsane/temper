# ADR-0036: Channel Transports

- Status: Accepted
- Date: 2026-03-24
- Deciders: Temper core maintainers
- Related:
  - ADR-0012: OAuth2, Webhooks, Timers, Secret Templates
  - `crates/temper-server/src/webhooks/receiver.rs` (inbound webhook pattern)
  - `crates/temper-server/src/adapters/openclaw.rs` (WebSocket reference)
  - `os-apps/temper-channels/specs/channel.ioa.toml` (TemperAgent entity spec)

## Context

The Channel entity currently supports external messaging platforms via HTTP webhooks and slash commands. This requires ngrok tunnels for local development and produces an unnatural UX (users must type `/ask` instead of just sending a message).

Platforms like Discord, Slack, and Teams offer persistent WebSocket connections (Discord Gateway, Slack RTM/Socket Mode) that allow servers to receive messages without exposing a public HTTP endpoint. These connections are outbound from the server, eliminating the need for tunnels.

The existing `AgentAdapter` trait is request-response (entity transition triggers outbound call). Channel transports are the reverse: persistent inbound event sources that produce entity transitions. The webhook receiver is the closest analogy, but for persistent connections instead of HTTP callbacks.

## Decision

### Sub-Decision 1: Channel transports as server-level infrastructure

Channel transports live in `crates/temper-server/src/channels/`. Each transport is a file in that module (e.g., `discord.rs`, `slack.rs`). They are spawned as background tasks during `temper serve` startup, following the same pattern as `spawn_optimization_loop` and `spawn_actor_passivation_loop`.

**Why this approach**: Transports are server-wide (one WebSocket per bot token), not entity-scoped. They don't fit the `AgentAdapter` trait (wrong direction) or `[[integration]]` specs (wrong lifecycle). Background tasks are the established pattern for long-lived server infrastructure.

### Sub-Decision 2: Transports own all platform I/O

The transport handles both inbound (receive events → dispatch `ReceiveMessage`) and outbound (watch for `SendReply` → deliver via platform API). WASM modules never call platform-specific APIs.

**Why this approach**: Keeps specs and WASM platform-agnostic. The same `send_reply` WASM works for Discord, Slack, WhatsApp — it records the reply content on entity state, and the transport delivers it. Adding a new platform means adding one Rust file, not touching any specs or WASM.

### Sub-Decision 3: No premature Connector trait

Discord is the first transport. We do not define a `Connector` trait until we have 2-3 implementations and can discover the common pattern from concrete code. Each transport is a standalone struct with a `run()` method.

**Why this approach**: Premature abstraction leads to wrong abstractions. Build Discord, build Slack, then extract the common interface.

### Sub-Decision 4: Configuration via CLI flags and environment variables

Each transport is activated by a CLI flag (e.g., `--discord-bot-token`) that also reads from environment variables (e.g., `DISCORD_BOT_TOKEN`). The token is stored in SecretsVault at startup for WASM access.

**Why this approach**: Simplest possible UX. `temper serve --discord-bot-token $TOKEN` and you're done.

## Rollout Plan

1. **Phase 0 (This PR)** — Discord transport: Gateway WebSocket, inbound routing, outbound reply delivery, CLI integration.
2. **Phase 1 (Follow-up)** — Guild channel support (not just DMs), @mention filtering.
3. **Phase 2 (Future)** — Slack transport as second implementation, then extract common patterns.

## Consequences

### Positive
- Natural DM UX — users message the bot directly, no slash commands
- No ngrok/tunnel dependency for local development
- Platform-agnostic WASM modules — one `send_reply` for all channels
- Clean pattern for future transports (Slack, Teams, WhatsApp)

### Negative
- Persistent WebSocket requires reconnection logic and heartbeat management
- Each transport adds server startup complexity (one more background task)

### Risks
- Discord Gateway requires MESSAGE_CONTENT privileged intent (must be enabled in Discord Developer Portal for bots in 100+ guilds)
- WebSocket disconnects during LLM processing could lose reply delivery (mitigated by retry on reconnect)

### DST Compliance
- Channel transport code uses `tokio::spawn` and `tokio-tungstenite` WebSocket — annotated with `// determinism-ok: WebSocket for channel transport`
- No simulation-visible state affected; transports operate outside the actor system's deterministic core

## Non-Goals

- Voice/audio channel support
- Discord sharding (not needed until 2500+ guilds)
- Connector trait abstraction (deferred until 2+ transports exist)
- Modifying the TemperAgent entity spec (already platform-agnostic)

## Alternatives Considered

1. **AgentAdapter implementation** — Rejected. The adapter trait is request-response (outbound). Discord Gateway is a persistent inbound event source. Wrong abstraction.
2. **Spec-driven `[[integration]] type = "discord_gateway"`** — Rejected. WebSocket connections are server-wide, not entity-scoped. The integration mechanism triggers per-transition, which makes no sense for a persistent connection.
3. **Separate `temper-discord` crate** — Rejected. Discord is just one channel type, not special enough to warrant its own crate. Lives alongside future transports in `channels/`.
4. **Webhook + ngrok approach** — Rejected by user as hacky. Requires exposing a public endpoint and running a tunnel for local development.
