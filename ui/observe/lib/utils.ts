const SENSITIVE_KEYS = new Set([
  "authorization",
  "api_key",
  "apikey",
  "token",
  "secret",
  "password",
  "cookie",
  "credential",
  "key",
  "bearer",
  "jwt",
  "session_token",
  "access_token",
  "refresh_token",
  "private_key",
]);

function isSensitiveKey(key: string): boolean {
  return SENSITIVE_KEYS.has(key.toLowerCase());
}

/** Recursively redact sensitive fields from an object (returns a new object). */
export function redactSensitiveFields(obj: unknown): unknown {
  if (obj === null || obj === undefined) return obj;
  if (typeof obj !== "object") return obj;
  if (Array.isArray(obj)) return obj.map(redactSensitiveFields);

  const result: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj as Record<string, unknown>)) {
    if (isSensitiveKey(key)) {
      result[key] = "[redacted]";
    } else if (typeof value === "object" && value !== null) {
      result[key] = redactSensitiveFields(value);
    } else {
      result[key] = value;
    }
  }
  return result;
}

/** Map a success rate (0–100) to a text color class. */
export function rateColor(rate: number): string {
  if (rate >= 80) return "text-teal-400";
  if (rate >= 50) return "text-amber-400";
  return "text-pink-400";
}

/** Map a success rate (0–100) to a background color class. */
export function rateBgColor(rate: number): string {
  if (rate >= 80) return "bg-teal-400";
  if (rate >= 50) return "bg-amber-400";
  return "bg-pink-400";
}

/** Generate a Cedar policy preview matching the Rust generate_policy() logic. */
export function generatePolicyPreview(
  agentId: string,
  action: string,
  resourceType: string,
  resourceId: string,
  scope: "narrow" | "medium" | "broad",
): string {
  switch (scope) {
    case "narrow":
      return `permit(\n  principal == Agent::"${agentId}",\n  action == Action::"${action}",\n  resource == ${resourceType}::"${resourceId}"\n);`;
    case "medium":
      return `permit(\n  principal == Agent::"${agentId}",\n  action == Action::"${action}",\n  resource is ${resourceType}\n);`;
    case "broad":
      return `permit(\n  principal == Agent::"${agentId}",\n  action,\n  resource is ${resourceType}\n);`;
  }
}

/** Text color class for a success rate percentage. */
export function rateColor(rate: number): string {
  if (rate >= 80) return "text-teal-400";
  if (rate >= 50) return "text-amber-400";
  return "text-pink-400";
}

/** Background color class for a success rate bar. */
export function rateBgColor(rate: number): string {
  if (rate >= 80) return "bg-teal-400";
  if (rate >= 50) return "bg-amber-400";
  return "bg-pink-400";
}

/** Group items into "Today" / "Yesterday" / "Older" buckets by date. */
export function groupByDate<T>(
  items: T[],
  getDate: (item: T) => string | undefined,
): Map<string, T[]> {
  const now = new Date();
  const todayStr = now.toDateString();
  const yesterday = new Date(now);
  yesterday.setDate(yesterday.getDate() - 1);
  const yesterdayStr = yesterday.toDateString();

  const groups = new Map<string, T[]>();

  for (const item of items) {
    const dateStr = getDate(item);
    if (!dateStr) {
      const bucket = "Older";
      if (!groups.has(bucket)) groups.set(bucket, []);
      groups.get(bucket)!.push(item);
      continue;
    }
    const itemDate = new Date(dateStr).toDateString();
    let bucket: string;
    if (itemDate === todayStr) {
      bucket = "Today";
    } else if (itemDate === yesterdayStr) {
      bucket = "Yesterday";
    } else {
      bucket = "Older";
    }
    if (!groups.has(bucket)) groups.set(bucket, []);
    groups.get(bucket)!.push(item);
  }

  // Return in order: Today, Yesterday, Older (only non-empty)
  const ordered = new Map<string, T[]>();
  for (const key of ["Today", "Yesterday", "Older"]) {
    const vals = groups.get(key);
    if (vals && vals.length > 0) ordered.set(key, vals);
  }
  return ordered;
}
