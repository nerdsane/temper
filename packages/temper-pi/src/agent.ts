import type { TemperAgentConfig, SpawnEvent, TemperToolConfig } from "./types.js";
import { createTemperTool } from "./temper-tool.js";

/**
 * Create a Temper tool config for programmatic (non-CLI) use.
 *
 * For interactive use, prefer the Pi extension:
 *   pi --no-tools --no-extensions -e src/extension.ts
 *
 * This function is for scripts (like demo/run.ts) that call the REPL
 * directly without an LLM in the loop.
 */
export function createTemperToolForAgent(config: TemperAgentConfig) {
  const toolConfig: TemperToolConfig = {
    temperUrl: config.temperUrl,
    agentHeaders: {
      "X-Temper-Principal-Id": config.principalId,
      "X-Temper-Principal-Kind": "agent",
      "X-Temper-Agent-Role": config.role,
      "X-Tenant-Id": config.tenant,
    },
  };

  return createTemperTool(toolConfig);
}

/**
 * Watch for Effect::Spawn events on the Temper server's SSE stream
 * and invoke the onSpawn callback for each.
 */
export async function watchForSpawns(
  temperUrl: string,
  tenant: string,
  onSpawn: (event: SpawnEvent) => void | Promise<void>,
  signal?: AbortSignal,
): Promise<void> {
  const url = `${temperUrl}/tdata/$events`;
  const response = await fetch(url, {
    headers: {
      Accept: "text/event-stream",
      "X-Tenant-Id": tenant,
    },
    signal,
  });

  if (!response.body) return;

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";

    for (const line of lines) {
      if (!line.startsWith("data: ")) continue;
      try {
        const data = JSON.parse(line.slice(6));
        if (data.event_type === "entity_created" && data.spawned_by) {
          await onSpawn({
            childEntityType: data.entity_type,
            childEntityId: data.entity_id,
            childRole: data.role ?? "child_agent",
            parentEntityType: data.spawned_by.entity_type,
            parentEntityId: data.spawned_by.entity_id,
          });
        }
      } catch {
        // Skip malformed events
      }
    }
  }
}
