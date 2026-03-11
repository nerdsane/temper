import { describe, it, expect } from "vitest";
import {
  redactSensitiveFields,
  rateColor,
  rateBgColor,
  generatePolicyPreview,
  groupByDate,
} from "@/lib/utils";

describe("redactSensitiveFields", () => {
  it("redacts known sensitive keys", () => {
    const input = { api_key: "secret123", name: "test" };
    const result = redactSensitiveFields(input) as Record<string, unknown>;
    expect(result.api_key).toBe("[redacted]");
    expect(result.name).toBe("test");
  });

  it("handles nested objects", () => {
    const input = { config: { token: "abc", url: "https://example.com" } };
    const result = redactSensitiveFields(input) as Record<string, Record<string, unknown>>;
    expect(result.config.token).toBe("[redacted]");
    expect(result.config.url).toBe("https://example.com");
  });

  it("handles arrays", () => {
    const input = [{ password: "pass" }, { name: "test" }];
    const result = redactSensitiveFields(input) as Record<string, unknown>[];
    expect(result[0].password).toBe("[redacted]");
    expect(result[1].name).toBe("test");
  });

  it("handles null and primitives", () => {
    expect(redactSensitiveFields(null)).toBeNull();
    expect(redactSensitiveFields(undefined)).toBeUndefined();
    expect(redactSensitiveFields("string")).toBe("string");
    expect(redactSensitiveFields(42)).toBe(42);
  });
});

describe("rateColor", () => {
  it("returns teal for rates >= 80", () => {
    expect(rateColor(80)).toBe("text-[var(--color-accent-teal)]");
    expect(rateColor(100)).toBe("text-[var(--color-accent-teal)]");
  });

  it("returns pink for rates >= 50 and < 80", () => {
    expect(rateColor(50)).toBe("text-[var(--color-accent-pink)]");
    expect(rateColor(79)).toBe("text-[var(--color-accent-pink)]");
  });

  it("returns pink for rates < 50", () => {
    expect(rateColor(0)).toBe("text-[var(--color-accent-pink)]");
    expect(rateColor(49)).toBe("text-[var(--color-accent-pink)]");
  });
});

describe("rateBgColor", () => {
  it("returns teal bg for rates >= 80", () => {
    expect(rateBgColor(80)).toBe("bg-[var(--color-accent-teal)]");
  });

  it("returns pink bg for rates >= 50 and < 80", () => {
    expect(rateBgColor(50)).toBe("bg-[var(--color-accent-pink)]");
  });

  it("returns pink bg for rates < 50", () => {
    expect(rateBgColor(0)).toBe("bg-[var(--color-accent-pink)]");
  });
});

describe("generatePolicyPreview", () => {
  it("generates narrow scope policy", () => {
    const result = generatePolicyPreview("agent-1", "Read", "Document", "doc-1", "narrow");
    expect(result).toContain('principal == Agent::"agent-1"');
    expect(result).toContain('action == Action::"Read"');
    expect(result).toContain('resource == Document::"doc-1"');
  });

  it("generates medium scope policy", () => {
    const result = generatePolicyPreview("agent-1", "Read", "Document", "doc-1", "medium");
    expect(result).toContain('resource is Document');
    expect(result).not.toContain('"doc-1"');
  });

  it("generates broad scope policy", () => {
    const result = generatePolicyPreview("agent-1", "Read", "Document", "doc-1", "broad");
    expect(result).toContain("action,");
    expect(result).toContain('resource is Document');
  });
});

describe("groupByDate", () => {
  it("groups items into Today/Yesterday/Older", () => {
    const now = new Date();
    const yesterday = new Date(now);
    yesterday.setDate(yesterday.getDate() - 1);
    const older = new Date(now);
    older.setDate(older.getDate() - 5);

    const items = [
      { date: now.toISOString(), label: "today" },
      { date: yesterday.toISOString(), label: "yesterday" },
      { date: older.toISOString(), label: "older" },
    ];

    const groups = groupByDate(items, (i) => i.date);
    expect(groups.get("Today")?.length).toBe(1);
    expect(groups.get("Yesterday")?.length).toBe(1);
    expect(groups.get("Older")?.length).toBe(1);
  });

  it("handles items with no date", () => {
    const items = [{ date: undefined as string | undefined, label: "no-date" }];
    const groups = groupByDate(items, (i) => i.date);
    expect(groups.get("Older")?.length).toBe(1);
  });
});
