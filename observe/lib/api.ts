import type {
  SpecSummary,
  SpecDetail,
  EntitySummary,
  VerificationResult,
  EntityHistory,
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

/** Check if the API server is reachable */
export async function checkConnection(): Promise<boolean> {
  try {
    const res = await fetch(`${API_BASE}/observe/specs`, { cache: "no-store" });
    return res.ok;
  } catch {
    return false;
  }
}
