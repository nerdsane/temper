import type { OpenClawPluginApi } from "openclaw/plugin-sdk";
import { writeFileSync, mkdirSync } from "fs";
import { homedir } from "os";
import { join } from "path";

type TemperOperation = "list" | "get" | "create" | "action" | "patch";

type TemperToolParams = {
  operation?: string;
  app?: string;
  entityType?: string;
  entityId?: string;
  actionName?: string;
  body?: unknown;
};

type TemperAppConfig = {
  agent: string;
  subscribe?: string[];
};

type TemperPluginConfig = {
  url: string;
  apps: Record<string, TemperAppConfig>;
  hooksToken?: string;
  hooksPort?: number;
};

type TemperEvent = {
  action?: string;
  entity_type?: string;
  entity_id?: string;
  from_status?: string;
  to_status?: string;
  [key: string]: unknown;
};

const DEFAULT_HOOKS_PORT = 18789;
const INITIAL_RECONNECT_MS = 1000;
const MAX_RECONNECT_MS = 30000;

const TEMPER_TOOL_PARAMETERS_SCHEMA = {
  type: "object",
  properties: {
    operation: {
      type: "string",
      enum: ["list", "get", "create", "action", "patch"],
      description: "Operation to perform",
    },
    app: {
      type: "string",
      description: "Tenant/app name (e.g. 'haku-ops')",
    },
    entityType: {
      type: "string",
      description: "Entity set name (e.g. 'Proposals', 'Tasks')",
    },
    entityId: {
      type: "string",
      description: "Entity ID (for get/action/patch)",
    },
    actionName: {
      type: "string",
      description: "Action name (for action operation, e.g. 'Approve')",
    },
    body: {
      type: "object",
      description: "Request body (for create/action/patch)",
    },
  },
  required: ["operation", "app", "entityType"],
} as const;

const textResult = (text: string) => ({
  content: [{ type: "text" as const, text }],
});

const errorResult = (text: string) => ({
  content: [{ type: "text" as const, text }],
  isError: true,
});

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const normalizeBaseUrl = (url: string): string => url.replace(/\/+$/, "");

const escapeODataString = (value: string): string => value.replace(/'/g, "''");

const isAbortError = (error: unknown): boolean =>
  typeof error === "object" &&
  error !== null &&
  "name" in error &&
  (error as { name?: unknown }).name === "AbortError";

const readStringParam = (params: Record<string, unknown>, key: string): string | undefined => {
  const value = params[key];
  if (typeof value !== "string") {
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
};

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

    apps[appName] = {
      agent,
      subscribe,
    };
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

  return {
    url: normalizeBaseUrl(url),
    apps,
    hooksToken,
    hooksPort,
  };
};

const buildEntityRef = (entityType: string, entityId?: string): string => {
  const encodedEntityType = encodeURIComponent(entityType);
  if (!entityId) {
    return encodedEntityType;
  }
  return `${encodedEntityType}('${escapeODataString(entityId)}')`;
};

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

const createTemperTool = (config: TemperPluginConfig) => ({
  name: "temper",
  description:
    "Query and act on Temper state machine entities. Operations: list, get, create, action, patch.",
  parameters: TEMPER_TOOL_PARAMETERS_SCHEMA,
  async execute(_toolCallId: string, params: TemperToolParams) {
    const paramsRecord = isRecord(params) ? params : {};

    const operation = readStringParam(paramsRecord, "operation");
    const app = readStringParam(paramsRecord, "app");
    const entityType = readStringParam(paramsRecord, "entityType");
    const entityId = readStringParam(paramsRecord, "entityId");
    const actionName = readStringParam(paramsRecord, "actionName");
    const body = paramsRecord.body;

    if (!operation || !["list", "get", "create", "action", "patch"].includes(operation)) {
      return errorResult("operation must be one of: list, get, create, action, patch");
    }
    if (!app) {
      return errorResult("app is required");
    }
    if (!entityType) {
      return errorResult("entityType is required");
    }

    const op = operation as TemperOperation;
    if ((op === "get" || op === "action" || op === "patch") && !entityId) {
      return errorResult("entityId is required for get/action/patch");
    }
    if (op === "action" && !actionName) {
      return errorResult("actionName is required for action");
    }
    if ((op === "create" || op === "patch") && !isRecord(body)) {
      return errorResult("body must be an object for create/patch");
    }
    if (op === "action" && body !== undefined && !isRecord(body)) {
      return errorResult("body must be an object when provided for action");
    }

    let method: "GET" | "POST" | "PATCH";
    let endpointPath: string;
    let requestBody: Record<string, unknown> | undefined;

    switch (op) {
      case "list": {
        method = "GET";
        endpointPath = buildEntityRef(entityType);
        break;
      }
      case "get": {
        method = "GET";
        endpointPath = buildEntityRef(entityType, entityId);
        break;
      }
      case "create": {
        method = "POST";
        endpointPath = buildEntityRef(entityType);
        requestBody = body as Record<string, unknown>;
        break;
      }
      case "action": {
        method = "POST";
        endpointPath = `${buildEntityRef(entityType, entityId)}/Temper.${encodeURIComponent(
          actionName as string,
        )}`;
        if (isRecord(body)) {
          requestBody = body;
        }
        break;
      }
      case "patch": {
        method = "PATCH";
        endpointPath = buildEntityRef(entityType, entityId);
        requestBody = body as Record<string, unknown>;
        break;
      }
    }

    const headers: Record<string, string> = {
      "X-Tenant-Id": app,
    };
    if (requestBody) {
      headers["Content-Type"] = "application/json";
    }

    const endpointUrl = `${config.url}/tdata/${endpointPath}`;

    try {
      const response = await fetch(endpointUrl, {
        method,
        headers,
        body: requestBody ? JSON.stringify(requestBody) : undefined,
      });

      const responseText = await response.text();
      if (!response.ok) {
        return errorResult(responseText || `${response.status} ${response.statusText}`);
      }
      return textResult(responseText);
    } catch (error) {
      return errorResult(error instanceof Error ? error.message : String(error));
    }
  },
});

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

  // Debounce: track last wake time per app so rapid events fire only one wake
  const lastWakeMs: Record<string, number> = {};
  const DEBOUNCE_MS = 30_000; // 30 seconds

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
        `Pull entity details: temper.get("${event.entity_type}", "${event.entity_id}")`,
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

      // 1. Write signal file (zero cost, persistent, always runs)
      if (agentId) {
        writeSignalFile(agentId, event, appName, text);
      }

      // 2. Fire wake — debounced per app (best-effort, one per 30s batch)
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

const temperPlugin = {
  id: "openclaw-temper",
  name: "Temper",
  description: "Temper state machine integration — real-time entity subscriptions and agent tools",
  register(api: OpenClawPluginApi) {
    let config: TemperPluginConfig;
    try {
      config = parseConfig(api.pluginConfig);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      api.logger.error(`[temper] ${message}`);
      api.registerTool({
        name: "temper",
        description:
          "Query and act on Temper state machine entities. Operations: list, get, create, action, patch.",
        parameters: TEMPER_TOOL_PARAMETERS_SCHEMA,
        async execute() {
          return errorResult(`[temper] Plugin misconfigured: ${message}`);
        },
      });
      return;
    }

    api.registerTool(createTemperTool(config));
    api.registerService(createSseService(api, config));

    api.on("gateway_start", () => {
      api.logger.info("[temper] Temper plugin ready");
    });
  },
};

export default temperPlugin;
