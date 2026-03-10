import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import { spawn, type ChildProcess } from "child_process";
import { writeFileSync, mkdirSync } from "fs";
import { homedir } from "os";
import { join } from "path";
import { createInterface, type Interface as ReadlineInterface } from "readline";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type TemperAppConfig = {
  agent: string;
  subscribe?: string[];
  specsDir?: string;
};

type TemperPluginConfig = {
  url: string;
  apps: Record<string, TemperAppConfig>;
  hooksToken?: string;
  hooksPort?: number;
  temperBinary?: string;
  port?: number;
  agentId?: string;
};

type ToolResult = {
  content: { type: "text"; text: string }[];
  isError?: boolean;
};

type TemperEvent = {
  action?: string;
  entity_type?: string;
  entity_id?: string;
  from_status?: string;
  to_status?: string;
  [key: string]: unknown;
};

type PendingRequest = {
  resolve: (result: ToolResult) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_HOOKS_PORT = 18789;
const INITIAL_RECONNECT_MS = 1000;
const MAX_RECONNECT_MS = 30000;
const MCP_CALL_TIMEOUT_MS = 180_000;
const DEBOUNCE_MS = 30_000;

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

const textResult = (text: string): ToolResult => ({
  content: [{ type: "text" as const, text }],
});

const errorResult = (text: string): ToolResult => ({
  content: [{ type: "text" as const, text }],
  isError: true,
});

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const normalizeBaseUrl = (url: string): string => url.replace(/\/+$/, "");

const isAbortError = (error: unknown): boolean =>
  typeof error === "object" &&
  error !== null &&
  "name" in error &&
  (error as { name?: unknown }).name === "AbortError";

const parseConfig = (pluginConfig: unknown): TemperPluginConfig => {
  if (!isRecord(pluginConfig)) {
    throw new Error("Invalid plugin config: expected an object");
  }

  const url = typeof pluginConfig.url === "string" ? pluginConfig.url.trim() : "";
  if (!url) {
    throw new Error("Invalid plugin config: url is required");
  }

  if (!isRecord(pluginConfig.apps)) {
    throw new Error("Invalid plugin config: apps is required");
  }

  const apps: Record<string, TemperAppConfig> = {};
  for (const [appName, rawAppConfig] of Object.entries(pluginConfig.apps)) {
    if (!isRecord(rawAppConfig)) {
      continue;
    }

    const agent = typeof rawAppConfig.agent === "string" ? rawAppConfig.agent.trim() : "";
    if (!agent) {
      continue;
    }

    const subscribe = Array.isArray(rawAppConfig.subscribe)
      ? rawAppConfig.subscribe
          .filter((entry): entry is string => typeof entry === "string")
          .map((entry) => entry.trim())
          .filter((entry) => entry.length > 0)
      : undefined;

    const specsDir =
      typeof rawAppConfig.specsDir === "string" && rawAppConfig.specsDir.trim().length > 0
        ? rawAppConfig.specsDir.trim()
        : undefined;

    apps[appName] = { agent, subscribe, specsDir };
  }

  if (Object.keys(apps).length === 0) {
    throw new Error("Invalid plugin config: apps must include at least one app with an agent");
  }

  const hooksToken =
    typeof pluginConfig.hooksToken === "string" && pluginConfig.hooksToken.trim().length > 0
      ? pluginConfig.hooksToken.trim()
      : undefined;

  const hooksPort =
    typeof pluginConfig.hooksPort === "number" && Number.isInteger(pluginConfig.hooksPort)
      ? pluginConfig.hooksPort
      : undefined;

  const temperBinary =
    typeof pluginConfig.temperBinary === "string" && pluginConfig.temperBinary.trim().length > 0
      ? pluginConfig.temperBinary.trim()
      : undefined;

  const port =
    typeof pluginConfig.port === "number" && Number.isInteger(pluginConfig.port)
      ? pluginConfig.port
      : undefined;

  const agentId =
    typeof pluginConfig.agentId === "string" && pluginConfig.agentId.trim().length > 0
      ? pluginConfig.agentId.trim()
      : undefined;

  return {
    url: normalizeBaseUrl(url),
    apps,
    hooksToken,
    hooksPort,
    temperBinary,
    port,
    agentId,
  };
};

// ---------------------------------------------------------------------------
// McpStdioBridge
// ---------------------------------------------------------------------------

class McpStdioBridge {
  private child: ChildProcess | null = null;
  private rl: ReadlineInterface | null = null;
  private nextId = 1;
  private pending = new Map<number, PendingRequest>();
  private initialized = false;
  private binary: string;
  private args: string[];
  private logger: { info: (msg: string) => void; warn: (msg: string) => void; error: (msg: string) => void };

  constructor(config: TemperPluginConfig, logger: McpStdioBridge["logger"]) {
    this.binary = config.temperBinary ?? "temper";
    this.logger = logger;

    const args = ["mcp"];
    if (config.port !== undefined) {
      args.push("--port", String(config.port));
    }
    if (config.agentId) {
      args.push("--agent-id", config.agentId);
    }
    // Note: --app flag is on `temper serve`, not `temper mcp`.
    // The MCP server is a thin client that connects to a running server.
    this.args = args;
  }

  async start(): Promise<void> {
    await this.spawn();
  }

  async stop(): Promise<void> {
    this.initialized = false;

    for (const [id, req] of this.pending) {
      clearTimeout(req.timer);
      req.reject(new Error("MCP bridge shutting down"));
      this.pending.delete(id);
    }

    if (this.rl) {
      this.rl.close();
      this.rl = null;
    }

    if (this.child) {
      const child = this.child;
      this.child = null;

      child.kill("SIGTERM");

      await new Promise<void>((resolve) => {
        const killTimer = setTimeout(() => {
          child.kill("SIGKILL");
          resolve();
        }, 3000);

        child.once("exit", () => {
          clearTimeout(killTimer);
          resolve();
        });
      });
    }
  }

  isReady(): boolean {
    return this.initialized && this.child !== null && this.child.exitCode === null;
  }

  async callTool(name: string, code: string): Promise<ToolResult> {
    if (!this.isReady()) {
      await this.restartIfNeeded();
    }

    if (!this.isReady()) {
      return errorResult("[temper] MCP bridge is not available");
    }

    const id = this.nextId++;

    return new Promise<ToolResult>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve(errorResult(`[temper] MCP call timed out after ${MCP_CALL_TIMEOUT_MS / 1000}s`));
      }, MCP_CALL_TIMEOUT_MS);

      this.pending.set(id, { resolve, reject, timer });

      this.sendRequest({
        jsonrpc: "2.0",
        id,
        method: "tools/call",
        params: {
          name,
          arguments: { code },
        },
      });
    });
  }

  private async spawn(): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      this.logger.info(`[temper] Spawning MCP bridge: ${this.binary} ${this.args.join(" ")}`);

      const child = spawn(this.binary, this.args, {
        stdio: ["pipe", "pipe", "pipe"],
        env: { ...process.env },
      });

      this.child = child;

      child.stderr?.on("data", (chunk: Buffer) => {
        const text = chunk.toString().trim();
        if (text) {
          this.logger.warn(`[temper] MCP stderr: ${text}`);
        }
      });

      child.on("error", (err) => {
        this.logger.error(`[temper] MCP process error: ${err.message}`);
        this.handleExit(-1);
        reject(err);
      });

      child.on("exit", (code) => {
        this.logger.warn(`[temper] MCP process exited with code ${code}`);
        this.handleExit(code ?? -1);
      });

      if (!child.stdout || !child.stdin) {
        reject(new Error("Failed to get stdio handles from MCP process"));
        return;
      }

      this.rl = createInterface({ input: child.stdout });
      this.rl.on("line", (line) => this.handleLine(line));

      // Send initialize handshake
      const initId = this.nextId++;
      const initTimer = setTimeout(() => {
        this.pending.delete(initId);
        reject(new Error("MCP initialize handshake timed out"));
      }, 10_000);

      this.pending.set(initId, {
        resolve: () => {
          clearTimeout(initTimer);
          this.pending.delete(initId);

          // Send initialized notification
          this.sendRequest({
            jsonrpc: "2.0",
            method: "notifications/initialized",
          });

          this.initialized = true;
          this.logger.info("[temper] MCP bridge initialized");
          resolve();
        },
        reject: (err) => {
          clearTimeout(initTimer);
          this.pending.delete(initId);
          reject(err);
        },
        timer: initTimer,
      });

      this.sendRequest({
        jsonrpc: "2.0",
        id: initId,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "openclaw-temper", version: "1.0.0" },
        },
      });
    });
  }

  private sendRequest(request: Record<string, unknown>): void {
    if (this.child?.stdin?.writable) {
      this.child.stdin.write(JSON.stringify(request) + "\n");
    }
  }

  private handleLine(line: string): void {
    const trimmed = line.trim();
    if (!trimmed) return;

    try {
      const msg = JSON.parse(trimmed);
      if (!isRecord(msg)) return;

      const id = typeof msg.id === "number" ? msg.id : undefined;
      if (id === undefined) return;

      const pending = this.pending.get(id);
      if (!pending) return;

      clearTimeout(pending.timer);
      this.pending.delete(id);

      if (isRecord(msg.error)) {
        const errMsg = typeof msg.error.message === "string" ? msg.error.message : "MCP error";
        pending.resolve(errorResult(`[temper] ${errMsg}`));
        return;
      }

      const result = msg.result;
      if (!isRecord(result)) {
        pending.resolve(textResult(JSON.stringify(result)));
        return;
      }

      // MCP tools/call result has { content: [...], isError?: bool }
      const content = result.content;
      if (Array.isArray(content)) {
        const texts: string[] = [];
        for (const item of content) {
          if (isRecord(item) && typeof item.text === "string") {
            texts.push(item.text);
          }
        }
        const isError = result.isError === true;
        if (isError) {
          pending.resolve(errorResult(texts.join("\n") || "Unknown MCP error"));
        } else {
          pending.resolve(textResult(texts.join("\n") || "(empty response)"));
        }
      } else {
        // Likely the initialize response — resolve with it
        pending.resolve(textResult(JSON.stringify(result)));
      }
    } catch {
      // Ignore unparseable lines (could be debug output)
    }
  }

  private handleExit(_code: number): void {
    this.initialized = false;
    this.child = null;

    if (this.rl) {
      this.rl.close();
      this.rl = null;
    }

    for (const [id, req] of this.pending) {
      clearTimeout(req.timer);
      req.reject(new Error("MCP process exited unexpectedly"));
      this.pending.delete(id);
    }
  }

  private async restartIfNeeded(): Promise<void> {
    if (this.isReady()) return;

    this.logger.info("[temper] MCP bridge not ready, restarting...");
    try {
      await this.stop();
      await this.spawn();
    } catch (err) {
      this.logger.error(
        `[temper] MCP bridge restart failed: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }
}

// ---------------------------------------------------------------------------
// Tool factories
// ---------------------------------------------------------------------------

const createSearchTool = (bridge: McpStdioBridge) => ({
  name: "temper_search",
  description: [
    "Discover loaded Temper specs and inspect entity types. Takes a `code` string — Python executed in a sandbox.",
    "",
    "This is a convenience wrapper that routes to the `execute` tool for read-only discovery operations.",
    "",
    "Available API:",
    "  await temper.specs(tenant)                          → loaded specs with states, actions, verification status",
    "  await temper.spec_detail(tenant, entity_type)       → full spec: actions, guards, invariants, state vars",
    "  await temper.get_agent_id(tenant)                   → current agent principal ID",
    "  await temper.list(tenant, entity_set)               → list entities (read-only query)",
    "  await temper.get(tenant, entity_set, entity_id)     → get single entity",
    "",
    "Example: return await temper.specs('default')",
  ].join("\n"),
  parameters: {
    type: "object",
    properties: {
      code: {
        type: "string",
        description: "Python code to execute in the Temper sandbox (discovery/read-only operations)",
      },
    },
    required: ["code"],
  } as const,
  async execute(_toolCallId: string, params: Record<string, unknown>) {
    const code = typeof params.code === "string" ? params.code : "";
    if (!code.trim()) {
      return errorResult("code parameter is required");
    }
    // The MCP server only exposes `execute` — route all calls through it.
    return bridge.callTool("execute", code);
  },
});

const createExecuteTool = (bridge: McpStdioBridge) => ({
  name: "temper_execute",
  description: [
    "Execute governed operations against a running Temper server. Takes a `code` string — Python executed in a sandbox.",
    "",
    "DISCOVERY:",
    "  await temper.specs(tenant)                                          → loaded specs with states, actions, verification status",
    "  await temper.spec_detail(tenant, entity_type)                      → full spec: actions, guards, invariants, state vars",
    "  await temper.get_agent_id(tenant)                                  → current agent principal ID",
    "",
    "ENTITY OPERATIONS:",
    "  await temper.list(tenant, entity_set, filter?)                     → list entities (optional OData $filter string)",
    "  await temper.get(tenant, entity_set, entity_id)                    → get entity by ID",
    "  await temper.create(tenant, entity_set, fields)                    → create entity",
    "  await temper.action(tenant, entity_set, entity_id, action, body)   → invoke action",
    "  await temper.patch(tenant, entity_set, entity_id, fields)          → update entity fields",
    "  await temper.navigate(tenant, path, params?)                       → raw OData navigation",
    "",
    "DEVELOPER:",
    "  await temper.submit_specs(tenant, {\"file.ioa.toml\": \"...\", \"model.csdl.xml\": \"...\"}) → submit specs",
    "  await temper.get_policies(tenant)                                  → Cedar policies",
    "  await temper.upload_wasm(tenant, module_name, wasm_path)           → upload WASM module",
    "  await temper.compile_wasm(tenant, module_name, rust_source)        → compile + upload WASM",
    "",
    "GOVERNANCE:",
    "  await temper.get_decisions(tenant, status?)                        → list decisions",
    "  await temper.get_decision_status(tenant, decision_id)              → check single decision",
    "  await temper.poll_decision(tenant, decision_id)                    → wait for human decision (120s timeout)",
    "",
    "EVOLUTION OBSERVABILITY:",
    "  await temper.get_trajectories(tenant, entity_type?, failed_only?, limit?) → trajectory spans",
    "  await temper.get_insights(tenant)                                  → evolution insights",
    "  await temper.get_evolution_records(tenant, record_type?)           → O-P-A-D-I records",
    "  await temper.check_sentinel(tenant)                                → trigger evolution engine",
    "",
    "Cedar governance: actions may be denied (403). Use poll_decision to wait for human approval.",
    "You cannot approve or set policies — only humans can do that.",
    "",
    "Example: return await temper.list('my-app', 'Tasks')",
  ].join("\n"),
  parameters: {
    type: "object",
    properties: {
      code: {
        type: "string",
        description: "Python code to execute in the Temper governed sandbox",
      },
    },
    required: ["code"],
  } as const,
  async execute(_toolCallId: string, params: Record<string, unknown>) {
    const code = typeof params.code === "string" ? params.code : "";
    if (!code.trim()) {
      return errorResult("code parameter is required");
    }
    return bridge.callTool("execute", code);
  },
});

// ---------------------------------------------------------------------------
// SSE service (unchanged from original)
// ---------------------------------------------------------------------------

const formatWakeText = (event: TemperEvent, appName: string): string => {
  const action = typeof event.action === "string" && event.action.length > 0 ? event.action : "unknown";
  const entityType =
    typeof event.entity_type === "string" && event.entity_type.length > 0
      ? event.entity_type
      : "unknown";
  const entityId =
    typeof event.entity_id === "string" && event.entity_id.length > 0 ? event.entity_id : "unknown";
  const fromStatus =
    typeof event.from_status === "string" && event.from_status.length > 0
      ? event.from_status
      : "unknown";
  const toStatus =
    typeof event.to_status === "string" && event.to_status.length > 0 ? event.to_status : "unknown";

  return `Temper: ${action} on ${entityType} ${entityId} (${fromStatus} → ${toStatus}) [app: ${appName}]`;
};

const parseSseEventData = (rawEvent: string): string | null => {
  const dataLines: string[] = [];
  for (const rawLine of rawEvent.split("\n")) {
    if (rawLine.startsWith("data:")) {
      dataLines.push(rawLine.slice(5).trimStart());
    }
  }

  if (dataLines.length === 0) {
    return null;
  }

  return dataLines.join("\n");
};

const createSseService = (api: OpenClawPluginApi, config: TemperPluginConfig) => {
  const hooksPort = config.hooksPort ?? DEFAULT_HOOKS_PORT;
  const hooksBaseUrl = `http://127.0.0.1:${hooksPort}/hooks`;
  const sseUrl = `${config.url}/tdata/$events`;

  let running = false;
  let reconnectDelayMs = INITIAL_RECONNECT_MS;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let reconnectResolver: (() => void) | null = null;
  let activeAbortController: AbortController | null = null;
  let loopPromise: Promise<void> | null = null;

  const waitForReconnect = async (delayMs: number): Promise<void> =>
    new Promise((resolve) => {
      reconnectResolver = resolve;
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        reconnectResolver = null;
        resolve();
      }, delayMs);
    });

  const lastWakeMs: Record<string, number> = {};

  const writeSignalFile = (agentId: string, event: TemperEvent, appName: string, text: string): void => {
    try {
      const signalDir = join(homedir(), "workspace", "shared-context", "signals", `for-${agentId}`);
      mkdirSync(signalDir, { recursive: true });
      const ts = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
      const entityId = typeof event.entity_id === "string" ? event.entity_id.slice(0, 12) : "unknown";
      const action = typeof event.action === "string" ? event.action : "event";
      const filename = `${ts}-${action}-${entityId}.md`;
      const content = [
        `# Temper Signal — ${new Date().toISOString()}`,
        ``,
        `**App:** ${appName}`,
        `**Event:** ${text}`,
        `**Entity ID:** ${event.entity_id ?? "unknown"}`,
        `**Entity Type:** ${event.entity_type ?? "unknown"}`,
        `**Action:** ${event.action ?? "unknown"}`,
        `**Transition:** ${event.from_status ?? "?"} → ${event.to_status ?? "?"}`,
        ``,
        `Pull entity details: temper_execute with code: return await temper.get("${appName}", "${event.entity_type}", "${event.entity_id}")`,
      ].join("\n");
      writeFileSync(join(signalDir, filename), content, "utf-8");
    } catch (err) {
      api.logger.warn(`[temper] Failed to write signal file: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  const postWake = async (text: string): Promise<void> => {
    const wakeUrl = `${hooksBaseUrl}/wake`;
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (config.hooksToken) {
      headers.Authorization = `Bearer ${config.hooksToken}`;
    }
    const response = await fetch(wakeUrl, {
      method: "POST",
      headers,
      body: JSON.stringify({ text, mode: "now" }),
    });
    if (!response.ok) {
      const errorBody = await response.text();
      throw new Error(`wake post failed (${response.status} ${response.statusText}): ${errorBody || "<empty body>"}`);
    }
  };

  const handleTemperEvent = async (event: TemperEvent): Promise<void> => {
    const entityType = typeof event.entity_type === "string" ? event.entity_type : "";
    if (!entityType) return;

    for (const [appName, appConfig] of Object.entries(config.apps)) {
      const subscriptions = appConfig.subscribe ?? [];
      if (!subscriptions.includes(entityType) && !subscriptions.includes("*")) continue;

      const agentId = appConfig.agent.trim();
      const text = formatWakeText(event, appName);

      if (agentId) {
        writeSignalFile(agentId, event, appName, text);
      }

      const now = Date.now();
      const lastWake = lastWakeMs[appName] ?? 0;
      if (now - lastWake >= DEBOUNCE_MS) {
        lastWakeMs[appName] = now;
        try {
          await postWake(text);
          api.logger.info(`[temper] Wake sent for ${appName}: ${text}`);
        } catch (error) {
          api.logger.warn(
            `[temper] Wake failed for "${appName}" (signal file written, will be picked up on next heartbeat): ${
              error instanceof Error ? error.message : String(error)
            }`,
          );
        }
      } else {
        api.logger.info(`[temper] Wake debounced for "${appName}" (${Math.round((DEBOUNCE_MS - (now - lastWake)) / 1000)}s remaining) — signal file written`);
      }
    }
  };

  const handleSseFrame = async (rawFrame: string): Promise<void> => {
    const payloadText = parseSseEventData(rawFrame);
    if (!payloadText) {
      return;
    }

    try {
      const payload = JSON.parse(payloadText);
      if (!isRecord(payload)) {
        return;
      }
      await handleTemperEvent(payload as TemperEvent);
    } catch (error) {
      api.logger.warn(
        `[temper] Failed to parse SSE payload: ${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    }
  };

  const connectSseOnce = async (signal: AbortSignal): Promise<void> => {
    const response = await fetch(sseUrl, {
      method: "GET",
      headers: {
        Accept: "text/event-stream",
      },
      signal,
    });

    if (!response.ok) {
      const bodyText = await response.text();
      throw new Error(
        `SSE connect failed (${response.status} ${response.statusText}): ${bodyText || "<empty body>"}`,
      );
    }
    if (!response.body) {
      throw new Error("SSE connect failed: response body is missing");
    }

    api.logger.info(`[temper] SSE connected: ${sseUrl}`);
    reconnectDelayMs = INITIAL_RECONNECT_MS;

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    while (running) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      if (!value) {
        continue;
      }

      buffer = `${buffer}${decoder.decode(value, { stream: true })}`
        .replace(/\r\n/g, "\n")
        .replace(/\r/g, "\n");

      let frameBoundary = buffer.indexOf("\n\n");
      while (frameBoundary !== -1) {
        const frame = buffer.slice(0, frameBoundary);
        buffer = buffer.slice(frameBoundary + 2);
        if (frame.trim().length > 0) {
          await handleSseFrame(frame);
        }
        frameBoundary = buffer.indexOf("\n\n");
      }
    }

    buffer = `${buffer}${decoder.decode()}`.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
    if (buffer.trim().length > 0) {
      await handleSseFrame(buffer);
    }

    api.logger.warn("[temper] SSE disconnected");
  };

  const runLoop = async (): Promise<void> => {
    while (running) {
      activeAbortController = new AbortController();
      try {
        await connectSseOnce(activeAbortController.signal);
      } catch (error) {
        if (!running || isAbortError(error)) {
          break;
        }
        api.logger.warn(
          `[temper] SSE connection error: ${error instanceof Error ? error.message : String(error)}`,
        );
      } finally {
        activeAbortController = null;
      }

      if (!running) {
        break;
      }

      const delayMs = reconnectDelayMs;
      api.logger.info(`[temper] Reconnecting SSE in ${delayMs}ms`);
      await waitForReconnect(delayMs);
      reconnectDelayMs = Math.min(reconnectDelayMs * 2, MAX_RECONNECT_MS);
    }
  };

  return {
    id: "temper-sse",
    start: async () => {
      if (running) {
        return;
      }
      running = true;
      reconnectDelayMs = INITIAL_RECONNECT_MS;
      api.logger.info(`[temper] Starting SSE service: ${sseUrl}`);
      if (!config.hooksToken) {
        api.logger.warn(
          "[temper] hooksToken is not set; hook requests may be rejected if OpenClaw hooks auth is enabled",
        );
      }
      loopPromise = runLoop().catch((error) => {
        api.logger.error(
          `[temper] SSE service loop crashed: ${error instanceof Error ? error.message : String(error)}`,
        );
      });
    },
    stop: async () => {
      running = false;

      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      if (reconnectResolver) {
        reconnectResolver();
        reconnectResolver = null;
      }

      if (activeAbortController) {
        activeAbortController.abort();
      }

      if (loopPromise) {
        await loopPromise;
        loopPromise = null;
      }

      api.logger.info("[temper] SSE service stopped");
    },
  };
};

// ---------------------------------------------------------------------------
// Plugin registration
// ---------------------------------------------------------------------------

const temperPlugin = {
  id: "openclaw-temper",
  name: "Temper",
  description: "Temper MCP integration — governed REPL tools, real-time entity subscriptions",
  register(api: OpenClawPluginApi) {
    let config: TemperPluginConfig;
    try {
      config = parseConfig(api.pluginConfig);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      api.logger.error(`[temper] ${message}`);
      api.registerTool({
        name: "temper_execute",
        description: "Execute governed Temper operations (plugin misconfigured)",
        parameters: { type: "object", properties: { code: { type: "string" } }, required: ["code"] },
        async execute() {
          return errorResult(`[temper] Plugin misconfigured: ${message}`);
        },
      });
      return;
    }

    // Create MCP bridge
    const bridge = new McpStdioBridge(config, api.logger);

    // Register tools
    api.registerTool(createSearchTool(bridge));
    api.registerTool(createExecuteTool(bridge));

    // Register MCP bridge as a service (lifecycle management)
    api.registerService({
      id: "temper-mcp",
      start: async () => {
        try {
          await bridge.start();
          api.logger.info("[temper] MCP bridge started");
        } catch (err) {
          api.logger.error(
            `[temper] MCP bridge failed to start: ${err instanceof Error ? err.message : String(err)}`,
          );
        }
      },
      stop: async () => {
        await bridge.stop();
        api.logger.info("[temper] MCP bridge stopped");
      },
    });

    // Register SSE service (unchanged)
    api.registerService(createSseService(api, config));

    // Inject Temper context at the start of each agent turn
    if (typeof api.on === "function") {
      api.on("before_prompt_build", async () => {
        if (!bridge.isReady()) return;

        try {
          // Use the execute tool with temper.specs() for discovery
          const result = await bridge.callTool("execute", "return await temper.specs('default')");
          if (!result.isError && result.content.length > 0) {
            const summary = result.content[0].text;
            return {
              systemMessage: `[Temper] Loaded specs: ${summary}`,
            };
          }
        } catch {
          // Non-critical — skip injection silently
        }
        return undefined;
      });
    }

    api.on("gateway_start", () => {
      api.logger.info("[temper] Temper plugin ready (MCP bridge + SSE)");
    });
  },
};

export default temperPlugin;
