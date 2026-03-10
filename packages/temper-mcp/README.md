# @temper/mcp

MCP server for [Temper](https://temper.build) — the governed application platform.

## Quick Start

```bash
# Add to Claude Code
claude mcp add temper -- npx -y @temper/mcp --url https://api.temper.build

# With API key
claude mcp add temper -e TEMPER_API_KEY=sk-xxx -- npx -y @temper/mcp --url https://api.temper.build

# Self-hosted
claude mcp add temper -- npx -y @temper/mcp --url http://localhost:3000
```

## What is Temper?

Temper is a conversational application platform. Describe what you want through conversation — the system generates specs, verifies them, and deploys. The MCP server gives AI agents access to:

- **Execute Python** against your Temper instance with persistent state
- **List and install apps** from the OS app catalog
- **Full OData API access** for CRUD operations on entities

## Supported Platforms

- macOS (Apple Silicon, Intel)
- Linux (x64, ARM64)
- Windows (x64)
