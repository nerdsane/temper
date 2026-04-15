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
  if (rate >= 80) return "text-[var(--color-accent-teal)]";
  if (rate >= 50) return "text-[var(--color-accent-pink)]";
  return "text-[var(--color-accent-pink)]";
}

/** Map a success rate (0–100) to a background color class. */
export function rateBgColor(rate: number): string {
  if (rate >= 80) return "bg-[var(--color-accent-teal)]";
  if (rate >= 50) return "bg-[var(--color-accent-pink)]";
  return "bg-[var(--color-accent-pink)]";
}

/** Generate a Cedar policy preview from a PolicyScopeMatrix. */
export function generatePolicyPreview(
  agentId: string,
  action: string,
  resourceType: string,
  resourceId: string,
  matrix: import("./types").PolicyScopeMatrix,
): string {
  // Principal clause
  let principalClause: string;
  switch (matrix.principal) {
    case "this_agent":
      principalClause = `principal == Agent::"${agentId}"`;
      break;
    case "agents_with_role":
    case "agents_of_type":
    case "any_agent":
      principalClause = "principal is Agent";
      break;
  }

  // Action clause
  let actionClause: string;
  switch (matrix.action) {
    case "this_action":
      actionClause = `action == Action::"${action}"`;
      break;
    case "all_actions_on_type":
    case "all_actions":
      actionClause = "action";
      break;
  }

  // Resource clause
  let resourceClause: string;
  switch (matrix.resource) {
    case "this_resource":
      resourceClause = `resource == ${resourceType}::"${resourceId}"`;
      break;
    case "any_of_type":
      resourceClause = `resource is ${resourceType}`;
      break;
    case "any_resource":
      resourceClause = "resource";
      break;
  }

  // When conditions
  const conditions: string[] = [];
  if (matrix.principal === "agents_with_role" && matrix.role_value) {
    conditions.push(`context.role == "${matrix.role_value}"`);
  }
  if (matrix.principal === "agents_of_type" && matrix.agent_type_value) {
    conditions.push(`context.agentType == "${matrix.agent_type_value}"`);
  }
  if (matrix.duration === "session" && matrix.session_id) {
    conditions.push(`context.sessionId == "${matrix.session_id}"`);
  }

  const whenClause = conditions.length > 0
    ? `\nwhen { ${conditions.join(" && ")} }`
    : "";

  return `permit(\n  ${principalClause},\n  ${actionClause},\n  ${resourceClause}\n)${whenClause};`;
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
