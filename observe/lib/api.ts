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
} from "./types";

const API_BASE = process.env.NEXT_PUBLIC_API_URL || "";

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
  const res = await fetchWithRetry(`${API_BASE}/observe/sentinel/check`, {
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
