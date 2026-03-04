/**
 * Temper SDK — thin HTTP client for Temper entity operations.
 *
 * Mirrors the dispatch surface of temper-mcp: entity CRUD, governance,
 * spec management, and SSE event streaming.
 */

/** Configuration for constructing a {@link TemperClient}. */
export interface TemperClientConfig {
  /** Base URL of the Temper server (e.g., "http://localhost:4200"). */
  baseUrl: string;
  /** Tenant ID for multi-tenant scoping. Defaults to "default". */
  tenant?: string;
  /** Optional principal ID for Cedar authorization headers. */
  principal?: string;
}

/** Authorization response from POST /api/authorize. */
export interface AuthzResponse {
  /** Whether the action was allowed by Cedar policy. */
  allowed: boolean;
  /** Decision ID for pending/escalated decisions. */
  decision_id?: string;
  /** Human-readable reason for the decision. */
  reason?: string;
}

/** A server-sent event representing an entity state change. */
export interface EntityEvent {
  /** The entity type (e.g., "Tasks", "Agents"). */
  entity_type: string;
  /** The entity ID. */
  entity_id: string;
  /** The action or transition that occurred. */
  action: string;
  /** The event payload. */
  data: unknown;
}

/**
 * Thin HTTP client for Temper server entity operations.
 *
 * @example
 * ```ts
 * const client = new TemperClient({ baseUrl: "http://localhost:4200" });
 * const tasks = await client.list("Tasks");
 * await client.create("Tasks", { id: "t-1" });
 * await client.action("Tasks", "t-1", "Start", {});
 * ```
 */
export class TemperClient {
  private baseUrl: string;
  private tenant: string;
  private principal?: string;

  constructor(config: TemperClientConfig) {
    this.baseUrl = config.baseUrl.replace(/\/+$/, "");
    this.tenant = config.tenant ?? "default";
    this.principal = config.principal;
  }

  // ── Entity CRUD ──────────────────────────────────────────────────

  /** List all entities of the given type. */
  async list(entityType: string): Promise<unknown[]> {
    const url = `${this.baseUrl}/tdata/${entityType}`;
    const resp = await fetch(url, {
      headers: this.defaultHeaders(),
    });
    this.checkStatus(resp, "list", entityType);
    const body = await resp.json();
    return body?.value ?? [];
  }

  /** Get a single entity by type and ID. */
  async get(entityType: string, id: string): Promise<unknown> {
    const url = `${this.baseUrl}/tdata/${entityType}('${id}')`;
    const resp = await fetch(url, {
      headers: this.defaultHeaders(),
    });
    this.checkStatus(resp, "get", entityType);
    return resp.json();
  }

  /** Create a new entity. */
  async create(
    entityType: string,
    fields: Record<string, unknown>
  ): Promise<unknown> {
    const url = `${this.baseUrl}/tdata/${entityType}`;
    const headers: Record<string, string> = {
      ...this.defaultHeaders(),
      "Content-Type": "application/json",
    };
    if (this.principal) {
      headers["x-temper-principal-id"] = this.principal;
      headers["x-temper-principal-kind"] = "Agent";
    }
    const resp = await fetch(url, {
      method: "POST",
      headers,
      body: JSON.stringify(fields),
    });
    this.checkStatus(resp, "create", entityType);
    return resp.json();
  }

  /** Invoke an OData action on an entity. */
  async action(
    entityType: string,
    id: string,
    actionName: string,
    params?: Record<string, unknown>
  ): Promise<unknown> {
    const url = `${this.baseUrl}/tdata/${entityType}('${id}')/Temper.${actionName}`;
    const resp = await fetch(url, {
      method: "POST",
      headers: {
        ...this.defaultHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify(params ?? {}),
    });
    this.checkStatus(resp, "action", `${entityType}.${actionName}`);
    return resp.json();
  }

  /** Patch (update) an existing entity's fields. */
  async patch(
    entityType: string,
    id: string,
    fields: Record<string, unknown>
  ): Promise<unknown> {
    const url = `${this.baseUrl}/tdata/${entityType}('${id}')`;
    const resp = await fetch(url, {
      method: "PATCH",
      headers: {
        ...this.defaultHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify(fields),
    });
    this.checkStatus(resp, "patch", entityType);
    return resp.json();
  }

  // ── Governance ───────────────────────────────────────────────────

  /** Check Cedar authorization for an action. */
  async authorize(
    agentId: string,
    actionName: string,
    resourceType: string,
    resourceId: string
  ): Promise<AuthzResponse> {
    const url = `${this.baseUrl}/api/authorize`;
    const resp = await fetch(url, {
      method: "POST",
      headers: {
        ...this.defaultHeaders(),
        "Content-Type": "application/json",
        "x-temper-principal-id": agentId,
        "x-temper-principal-kind": "Agent",
      },
      body: JSON.stringify({
        agent_id: agentId,
        action: actionName,
        resource_type: resourceType,
        resource_id: resourceId,
      }),
    });
    return resp.json() as Promise<AuthzResponse>;
  }

  // ── SSE ──────────────────────────────────────────────────────────

  /**
   * Watch for entity events via SSE.
   *
   * Yields {@link EntityEvent} objects as they arrive from the server.
   * Optionally filter by entity type.
   */
  async *watchEvents(filter?: {
    entityType?: string;
  }): AsyncGenerator<EntityEvent> {
    const url = `${this.baseUrl}/api/events`;
    const resp = await fetch(url, {
      headers: {
        ...this.defaultHeaders(),
        Accept: "text/event-stream",
        "Cache-Control": "no-cache",
      },
    });

    if (!resp.ok || !resp.body) {
      throw new Error(`SSE connection failed: ${resp.status}`);
    }

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });

        // Process complete lines.
        let newlineIdx: number;
        while ((newlineIdx = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, newlineIdx).replace(/\r$/, "");
          buffer = buffer.slice(newlineIdx + 1);

          // Skip comments and empty lines.
          if (line.startsWith(":") || line === "") continue;

          if (line.startsWith("data:")) {
            const data = line.slice(5).trim();
            if (!data) continue;

            try {
              const event = JSON.parse(data) as EntityEvent;
              if (
                filter?.entityType &&
                event.entity_type !== filter.entityType
              ) {
                continue;
              }
              yield event;
            } catch {
              // Skip malformed JSON.
            }
          }
        }
      }
    } finally {
      reader.releaseLock();
    }
  }

  // ── Helpers ────────────────────────────────────────────────────

  /** Build common headers for all requests. */
  private defaultHeaders(): Record<string, string> {
    return {
      "x-tenant-id": this.tenant,
    };
  }

  /** Throw on non-2xx responses. */
  private checkStatus(resp: Response, operation: string, context: string) {
    if (!resp.ok) {
      throw new Error(
        `${operation} ${context} failed with status ${resp.status}`
      );
    }
  }

  /** Build the entity collection URL. */
  entityUrl(entityType: string): string {
    return `${this.baseUrl}/tdata/${entityType}`;
  }

  /** Build the entity instance URL. */
  entityInstanceUrl(entityType: string, id: string): string {
    return `${this.baseUrl}/tdata/${entityType}('${id}')`;
  }

  /** Build the action URL. */
  actionUrl(entityType: string, id: string, actionName: string): string {
    return `${this.baseUrl}/tdata/${entityType}('${id}')/Temper.${actionName}`;
  }
}
