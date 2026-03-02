/** Configuration for the Temper REPL tool. */
export interface TemperToolConfig {
  /** Base URL of the Temper server (e.g., "http://localhost:4200") */
  temperUrl: string;
  /** Headers sent with every REPL request for agent identity */
  agentHeaders: Record<string, string>;
}

/** Request body for POST /api/repl */
export interface ReplRequest {
  code: string;
}

/** Response from POST /api/repl */
export interface ReplResponse {
  result: unknown;
  error: string | null;
}

/** Configuration for creating a Temper-governed agent session. */
export interface TemperAgentConfig {
  /** Base URL of the Temper server */
  temperUrl: string;
  /** Tenant ID for multi-tenant scoping */
  tenant: string;
  /** Unique principal ID for this agent (used in Cedar policies) */
  principalId: string;
  /** Agent role (e.g., "lead_agent", "test_agent", "review_agent") */
  role: string;
  /** LLM model identifier (e.g., "claude-sonnet-4-20250514", "gpt-4o") */
  model: string;
  /** LLM provider (e.g., "anthropic", "openai"). Defaults based on model name. */
  provider?: string;
  /** The task description/prompt for the agent */
  task: string;
  /** Optional system prompt override. If not provided, a default is generated. */
  systemPrompt?: string;
  /** Callback when Effect::Spawn creates a child entity */
  onSpawn?: (event: SpawnEvent) => void | Promise<void>;
}

/** Event emitted when Effect::Spawn creates a child entity. */
export interface SpawnEvent {
  /** The spawned child entity's type (e.g., "TestWorkflow") */
  childEntityType: string;
  /** The spawned child entity's ID */
  childEntityId: string;
  /** The role assigned to the child agent */
  childRole: string;
  /** The parent entity that triggered the spawn */
  parentEntityType: string;
  parentEntityId: string;
}

