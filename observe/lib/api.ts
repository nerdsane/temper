import {
  MOCK_SPECS,
  MOCK_SPEC_DETAILS,
  MOCK_ENTITIES,
  MOCK_VERIFICATION,
  MOCK_ENTITY_HISTORY,
  type SpecSummary,
  type SpecDetail,
  type EntitySummary,
  type VerificationResult,
  type EntityHistory,
} from "./mock-data";

const API_BASE = process.env.NEXT_PUBLIC_API_URL || "";

export async function fetchSpecs(): Promise<SpecSummary[]> {
  try {
    const res = await fetch(`${API_BASE}/observe/specs`, {
      cache: "no-store",
    });
    if (!res.ok) throw new Error("API unavailable");
    return await res.json();
  } catch {
    return MOCK_SPECS;
  }
}

export async function fetchSpecDetail(entity: string): Promise<SpecDetail | null> {
  try {
    const res = await fetch(`${API_BASE}/observe/specs/${entity}`, {
      cache: "no-store",
    });
    if (!res.ok) throw new Error("API unavailable");
    return await res.json();
  } catch {
    return MOCK_SPEC_DETAILS[entity] || null;
  }
}

export async function fetchEntities(): Promise<EntitySummary[]> {
  try {
    const res = await fetch(`${API_BASE}/observe/entities`, {
      cache: "no-store",
    });
    if (!res.ok) throw new Error("API unavailable");
    return await res.json();
  } catch {
    return MOCK_ENTITIES;
  }
}

export async function runVerification(entity: string): Promise<VerificationResult | null> {
  try {
    const res = await fetch(`${API_BASE}/observe/verify/${entity}`, {
      method: "POST",
      cache: "no-store",
    });
    if (!res.ok) throw new Error("API unavailable");
    return await res.json();
  } catch {
    return MOCK_VERIFICATION[entity] || null;
  }
}

export async function fetchEntityHistory(
  entityType: string,
  entityId: string
): Promise<EntityHistory | null> {
  try {
    const res = await fetch(
      `${API_BASE}/observe/entities/${entityType}/${entityId}/history`,
      { cache: "no-store" }
    );
    if (!res.ok) throw new Error("API unavailable");
    return await res.json();
  } catch {
    const key = `${entityType}:${entityId}`;
    return MOCK_ENTITY_HISTORY[key] || null;
  }
}
