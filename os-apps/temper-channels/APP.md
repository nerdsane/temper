# Temper Channels

Multi-platform messaging channel adapter with routing and session management. Connects Discord, Slack, webhooks, and other platforms to Temper agents. Routes incoming messages to agents based on configurable binding-tier rules and maintains session continuity per thread.

## Entity Types

### Channel

Manages connection lifecycle for a messaging platform channel. Receives messages, routes to agents, delivers replies.

**States**: Created → Connecting → Connected → Disconnected → Archived

**Key actions**:
- **Configure**: Set channel type, ID, guild, default agent config, and webhook details
- **Connect**: Start connection (triggers WASM)
- **Ready**: Connection established; channel can receive messages
- **ReceiveMessage**: Incoming message from the platform; triggers routing WASM
- **SendReply**: Send a reply back to the channel (triggers WASM)
- **ReplyDelivered**: Reply delivery confirmed
- **Disconnect / Reconnect**: Handle connection drops
- **Archive**: Terminal state
- **ConnectFailed / RouteFailed / ReplyFailed**: Error handling (non-terminal)

### ChannelSession

Maps channel threads to TemperAgent entities. Tracks active conversations for session continuity -- same thread keeps the same agent.

**States**: Active → Expired

**Key actions**:
- **Create**: Link a thread to an agent with channel, author, and timestamp
- **Resume**: Update timestamp on new message in existing session
- **Expire**: Session timeout or manual cleanup (terminal)

### AgentRoute

Binding-tier routing rules for channel messages. Routes incoming messages to agent configurations based on priority: peer > guild_roles > guild > team > channel.

**States**: Active → Disabled

**Key actions**:
- **Register**: Create a routing rule with binding tier, channel/guild IDs, match pattern, agent config, and soul
- **Update**: Modify routing configuration
- **Disable / Enable**: Toggle the route

## Setup

```
temper.install_app("<tenant>", "temper-channels")
```
