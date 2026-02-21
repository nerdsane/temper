# Codex Spec: Documentation Update

## Goal
Update `skills/temper/SKILL.md` and repo `README.md` to cover all new features.

## Depends on
Specs 01-04 being complete.

## Requirements

### SKILL.md additions

#### Storage Backends section
- Explain the three backends: Postgres, Turso, Redis
- Show CLI flag: `--storage postgres|turso|redis`
- Show env vars per backend
- When to use which:
  - Postgres: production, multi-tenant, existing infrastructure
  - Turso: edge deployment, embedded, low-ops (no Postgres to manage)
  - Redis: ephemeral/cache use cases
- Local dev with Turso: `--storage turso` with `TURSO_URL=file:local.db` (no cloud account needed)

#### Code Mode MCP section
- What it is and why (link to Cloudflare blog + Monty)
- Setup: `temper mcp --app my-app=path/to/specs --port 3001`
- Add to Claude Desktop / OpenClaw / Cursor config
- Example: search for entity types, then execute a compound operation
- Security model: Monty sandbox, only `temper.*` functions can touch network

#### OpenClaw Plugin section
- Install and configure
- Config example in `openclaw.json`
- How SSE events appear in agent sessions
- Using the `temper` tool from agent code
- Multi-agent setup

### README.md updates
- Add "Integrations" section: MCP, OpenClaw plugin
- Add "Storage Backends" section
- Update "Getting Started" to mention storage selection
- Add badges if applicable

### Do NOT
- Remove existing documentation
- Change the design system or showcase app docs
- Rewrite sections that are already correct
