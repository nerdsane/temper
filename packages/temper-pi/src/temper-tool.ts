import type { TemperToolConfig, ReplResponse } from "./types.js";

/**
 * Create a simple Temper REPL caller for programmatic use.
 *
 * For interactive agent use, prefer the Pi extension (src/extension.ts).
 * This is for scripts that call the REPL directly.
 */
export function createTemperTool(config: TemperToolConfig) {
  return {
    async execute(params: { code: string }): Promise<ReplResponse> {
      const response = await fetch(`${config.temperUrl}/api/repl`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          ...config.agentHeaders,
        },
        body: JSON.stringify({ code: params.code }),
      });
      return await response.json();
    },
  };
}
