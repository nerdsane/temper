import type {
  SpecSummary,
  SpecDetail,
  EntitySummary,
  VerificationResult,
  EntityHistory,
  AllVerificationStatus,
  DesignTimeEvent,
  EntityStateChange,
  WorkflowsResponse,
  TrajectoryResponse,
  EvolutionRecordsResponse,
  EvolutionInsightsResponse,
  SentinelCheckResponse,
  WasmModulesResponse,
  WasmInvocationsResponse,
  PendingDecision,
  DecisionsResponse,
  PolicyScope,
  AgentsResponse,
  AgentHistoryResponse,
  UnmetIntentsResponse,
  EvolutionRecordDetail,
  ExtendedSentinelCheckResponse,
  FeatureRequest,
  FeatureRequestDisposition,
} from "./types";

const API_BASE = process.env.NEXT_PUBLIC_API_URL || "";

/** Admin headers for Observe UI — trusted admin interface for governance decisions. */
const ADMIN_HEADERS: Record<string, string> = {
  "X-Temper-Principal-Id": "observe-ui",
  "X-Temper-Principal-Kind": "admin",
};

export class ApiError extends Error {
  constructor(
    message: string,
    public status: number,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

function isTransient(status: number): boolean {
  return status === 408 || status === 429 || status >= 500;
}

async function fetchWithRetry(input: string, init?: RequestInit): Promise<Response> {
  try {
    const res = await fetch(input, init);
    if (!res.ok && isTransient(res.status)) {
      await new Promise((r) => setTimeout(r, 1000));
      return fetch(input, init);
    }
    return res;
  } catch {
    // Network error — retry once after 1s
    await new Promise((r) => setTimeout(r, 1000));
    return fetch(input, init);
  }
}

export async function fetchSpecs(): Promise<SpecSummary[]> {
  const res = await fetchWithRetry(`${API_BASE}/observe/specs`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch specs: ${res.status}`, res.status);
  return res.json();
}

export async function fetchSpecDetail(entity: string): Promise<SpecDetail> {
  const res = await fetchWithRetry(`${API_BASE}/observe/specs/${encodeURIComponent(entity)}`, {
    cache: "no-store",
  });
  if (!res.ok) throw new ApiError(`Failed to fetch spec "${entity}": ${res.status}`, res.status);
  return res.json();
}

export async function fetchEntities(): Promise<EntitySummary[]> {
  const res = await fetchWithRetry(`${API_BASE}/observe/entities`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch entities: ${res.status}`, res.status);
  return res.json();
}

export async function runVerification(entity: string): Promise<VerificationResult> {
  const res = await fetchWithRetry(`${API_BASE}/observe/verify/${encodeURIComponent(entity)}`, {
    method: "POST",
    cache: "no-store",
  });
  if (!res.ok)
    throw new ApiError(`Verification failed for "${entity}": ${res.status}`, res.status);
  return res.json();
}

export async function fetchEntityHistory(
  entityType: string,
  entityId: string,
): Promise<EntityHistory> {
  const res = await fetchWithRetry(
    `${API_BASE}/observe/entities/${encodeURIComponent(entityType)}/${encodeURIComponent(entityId)}/history`,
    { cache: "no-store" },
  );
  if (!res.ok)
    throw new ApiError(
      `Failed to fetch history for ${entityType}/${entityId}: ${res.status}`,
      res.status,
    );
  return res.json();
}

/** Fetch all entity verification statuses */
export async function fetchVerificationStatus(): Promise<AllVerificationStatus> {
  const res = await fetchWithRetry(`${API_BASE}/observe/verification-status`, {
    cache: "no-store",
  });
  if (!res.ok)
    throw new ApiError(`Failed to fetch verification status: ${res.status}`, res.status);
  return res.json();
}

/** Fetch workflow view for all apps */
export async function fetchWorkflows(): Promise<WorkflowsResponse> {
  const res = await fetchWithRetry(`${API_BASE}/observe/workflows`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch workflows: ${res.status}`, res.status);
  return res.json();
}

/** Subscribe to design-time SSE events. Returns a cleanup function. */
export function subscribeDesignTimeEvents(
  onEvent: (event: DesignTimeEvent) => void,
): () => void {
  const source = new EventSource(`${API_BASE}/observe/design-time/stream`);
  source.addEventListener("design_time", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data) as DesignTimeEvent;
      onEvent(data);
    } catch {
      // Ignore parse errors
    }
  });
  return () => source.close();
}

/** Subscribe to runtime entity state change events. Returns a cleanup function. */
export function subscribeEntityEvents(
  onEvent: (event: EntityStateChange) => void,
): () => void {
  const source = new EventSource(`${API_BASE}/observe/events/stream`);
  source.addEventListener("state_change", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data) as EntityStateChange;
      onEvent(data);
    } catch {
      // Ignore parse errors
    }
  });
  return () => source.close();
}

/** Fetch trajectory stats and failed intents */
export async function fetchTrajectories(params?: {
  entity_type?: string;
  success?: string;
}): Promise<TrajectoryResponse> {
  const query = new URLSearchParams();
  if (params?.entity_type) query.set("entity_type", params.entity_type);
  if (params?.success) query.set("success", params.success);
  const qs = query.toString();
  const url = `${API_BASE}/observe/trajectories${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch trajectories: ${res.status}`, res.status);
  return res.json();
}

/** Fetch evolution record chain */
export async function fetchEvolutionRecords(params?: {
  record_type?: string;
  status?: string;
}): Promise<EvolutionRecordsResponse> {
  const query = new URLSearchParams();
  if (params?.record_type) query.set("record_type", params.record_type);
  if (params?.status) query.set("status", params.status);
  const qs = query.toString();
  const url = `${API_BASE}/observe/evolution/records${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch evolution records: ${res.status}`, res.status);
  return res.json();
}

/** Fetch ranked evolution insights */
export async function fetchEvolutionInsights(): Promise<EvolutionInsightsResponse> {
  const res = await fetchWithRetry(`${API_BASE}/observe/evolution/insights`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch insights: ${res.status}`, res.status);
  return res.json();
}

/** Trigger sentinel health check */
export async function triggerSentinelCheck(): Promise<SentinelCheckResponse> {
  const res = await fetchWithRetry(`${API_BASE}/api/evolution/sentinel/check`, {
    method: "POST",
    cache: "no-store",
  });
  if (!res.ok) throw new ApiError(`Sentinel check failed: ${res.status}`, res.status);
  return res.json();
}

/** Fetch Prometheus-format metrics as text */
export async function fetchMetricsText(): Promise<string> {
  const res = await fetchWithRetry(`${API_BASE}/observe/metrics`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch metrics: ${res.status}`, res.status);
  return res.text();
}

/** Fetch WASM modules with stats */
export async function fetchWasmModules(): Promise<WasmModulesResponse> {
  const res = await fetchWithRetry(`${API_BASE}/observe/wasm/modules`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch WASM modules: ${res.status}`, res.status);
  return res.json();
}

/** Fetch WASM invocation history */
export async function fetchWasmInvocations(params?: {
  module_name?: string;
  success?: boolean;
  limit?: number;
}): Promise<WasmInvocationsResponse> {
  const query = new URLSearchParams();
  if (params?.module_name) query.set("module_name", params.module_name);
  if (params?.success !== undefined) query.set("success", String(params.success));
  if (params?.limit) query.set("limit", String(params.limit));
  const qs = query.toString();
  const url = `${API_BASE}/observe/wasm/invocations${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch WASM invocations: ${res.status}`, res.status);
  return res.json();
}

/** Check if the API server is reachable */
export async function checkConnection(): Promise<boolean> {
  try {
    const res = await fetch(`${API_BASE}/observe/specs`, { cache: "no-store" });
    return res.ok;
  } catch {
    return false;
  }
}

/** Fetch pending/resolved decisions for a tenant */
export async function fetchDecisions(tenant: string, params?: {
  status?: string;
}): Promise<DecisionsResponse> {
  const query = new URLSearchParams();
  if (params?.status) query.set("status", params.status);
  const qs = query.toString();
  const url = `${API_BASE}/api/tenants/${encodeURIComponent(tenant)}/decisions${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store", headers: ADMIN_HEADERS });
  if (!res.ok) throw new ApiError(`Failed to fetch decisions: ${res.status}`, res.status);
  return res.json();
}

/** Approve a pending decision */
export async function approveDecision(tenant: string, decisionId: string, scope: PolicyScope): Promise<void> {
  const url = `${API_BASE}/api/tenants/${encodeURIComponent(tenant)}/decisions/${encodeURIComponent(decisionId)}/approve`;
  const res = await fetchWithRetry(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...ADMIN_HEADERS },
    body: JSON.stringify({ scope }),
  });
  if (!res.ok) throw new ApiError(`Failed to approve decision: ${res.status}`, res.status);
}

/** Deny a pending decision */
export async function denyDecision(tenant: string, decisionId: string): Promise<void> {
  const url = `${API_BASE}/api/tenants/${encodeURIComponent(tenant)}/decisions/${encodeURIComponent(decisionId)}/deny`;
  const res = await fetchWithRetry(url, { method: "POST", headers: ADMIN_HEADERS });
  if (!res.ok) throw new ApiError(`Failed to deny decision: ${res.status}`, res.status);
}

/** Subscribe to pending decision SSE stream */
export function subscribePendingDecisions(tenant: string, onEvent: (decision: PendingDecision) => void): () => void {
  const source = new EventSource(`${API_BASE}/api/tenants/${encodeURIComponent(tenant)}/decisions/stream`);
  source.addEventListener("pending_decision", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data) as PendingDecision;
      onEvent(data);
    } catch { /* ignore parse errors */ }
  });
  return () => source.close();
}

/** Fetch all decisions across all tenants */
export async function fetchAllDecisions(params?: {
  status?: string;
}): Promise<DecisionsResponse> {
  const query = new URLSearchParams();
  if (params?.status) query.set("status", params.status);
  const qs = query.toString();
  const url = `${API_BASE}/api/decisions${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store", headers: ADMIN_HEADERS });
  if (!res.ok) throw new ApiError(`Failed to fetch decisions: ${res.status}`, res.status);
  return res.json();
}

/** Subscribe to all pending decisions across all tenants */
export function subscribeAllPendingDecisions(onEvent: (decision: PendingDecision) => void): () => void {
  const source = new EventSource(`${API_BASE}/api/decisions/stream`);
  source.addEventListener("pending_decision", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data) as PendingDecision;
      onEvent(data);
    } catch { /* ignore parse errors */ }
  });
  return () => source.close();
}

/** Fetch agent list with stats */
export async function fetchAgents(params?: { tenant?: string }): Promise<AgentsResponse> {
  const query = new URLSearchParams();
  if (params?.tenant) query.set("tenant", params.tenant);
  const qs = query.toString();
  const url = `${API_BASE}/observe/agents${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch agents: ${res.status}`, res.status);
  return res.json();
}

/** Fetch agent action history */
export async function fetchAgentHistory(agentId: string, params?: {
  tenant?: string;
  entity_type?: string;
  limit?: number;
}): Promise<AgentHistoryResponse> {
  const query = new URLSearchParams();
  if (params?.tenant) query.set("tenant", params.tenant);
  if (params?.entity_type) query.set("entity_type", params.entity_type);
  if (params?.limit) query.set("limit", String(params.limit));
  const qs = query.toString();
  const url = `${API_BASE}/observe/agents/${encodeURIComponent(agentId)}/history${qs ? `?${qs}` : ""}`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch agent history: ${res.status}`, res.status);
  return res.json();
}

/** Fetch Cedar policies for a tenant */
export async function fetchPolicies(tenant: string): Promise<{ policy_text: string; policy_count: number }> {
  const url = `${API_BASE}/api/tenants/${encodeURIComponent(tenant)}/policies`;
  const res = await fetchWithRetry(url, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch policies: ${res.status}`, res.status);
  return res.json();
}

/** Fetch unmet intents from trajectory analysis */
export async function fetchUnmetIntents(): Promise<UnmetIntentsResponse> {
  const res = await fetchWithRetry(`${API_BASE}/observe/evolution/unmet-intents`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch unmet intents: ${res.status}`, res.status);
  return res.json();
}

/** Fetch full evolution record detail by ID */
export async function fetchRecordDetail(id: string): Promise<EvolutionRecordDetail> {
  const res = await fetchWithRetry(`${API_BASE}/observe/evolution/records/${encodeURIComponent(id)}`, { cache: "no-store" });
  if (!res.ok) throw new ApiError(`Failed to fetch record ${id}: ${res.status}`, res.status);
  const data = await res.json();
  return data.record || data;
}

/** Trigger sentinel check with extended response (includes insights) */
export async function triggerExtendedSentinelCheck(): Promise<ExtendedSentinelCheckResponse> {
  const res = await fetchWithRetry(`${API_BASE}/api/evolution/sentinel/check`, {
    method: "POST",
    cache: "no-store",
  });
  if (!res.ok) throw new ApiError(`Sentinel check failed: ${res.status}`, res.status);
  return res.json();
}

/** Subscribe to evolution SSE events */
export function subscribeEvolutionEvents(onEvent: (event: Record<string, unknown>) => void): () => void {
  const source = new EventSource(`${API_BASE}/observe/evolution/stream`);
  source.addEventListener("evolution_event", (e) => {
    try {
      const data = JSON.parse((e as MessageEvent).data);
      onEvent(data);
    } catch { /* ignore parse errors */ }
  });
  return () => source.close();
}

/** Fetch feature requests, optionally filtered by disposition */
export async function fetchFeatureRequests(disposition?: FeatureRequestDisposition): Promise<FeatureRequest[]> {
  const params = disposition ? `?disposition=${disposition}` : "";
  const res = await fetchWithRetry(`${API_BASE}/observe/evolution/feature-requests${params}`, { cache: "no-store" });
  if (!res.ok) return [];
  return res.json();
}

/** Update a feature request's disposition and/or developer notes */
export async function updateFeatureRequest(
  id: string,
  update: { disposition?: FeatureRequestDisposition; developer_notes?: string },
): Promise<boolean> {
  const res = await fetchWithRetry(`${API_BASE}/observe/evolution/feature-requests/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(update),
  });
  return res.ok;
}
