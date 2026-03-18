"use client";

import { useState, useMemo } from "react";
import type { PolicyEntry } from "@/lib/types";
import { useRelativeTime } from "@/lib/hooks";

interface PolicyCardProps {
  policy: PolicyEntry;
  onToggle: (policyId: string, enabled: boolean) => void;
  onDelete: (policyId: string) => void;
  onUpdate: (policyId: string, cedarText: string) => void;
  acting: boolean;
}

const SOURCE_BADGES: Record<string, { label: string; className: string }> = {
  "os-app": {
    label: "Base",
    className: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  },
  decision: {
    label: "Approved",
    className: "bg-purple-500/10 text-purple-400",
  },
  manual: {
    label: "Manual",
    className: "bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)]",
  },
  "migrated-legacy": {
    label: "Legacy",
    className: "bg-amber-500/10 text-amber-400",
  },
};

/** Best-effort extraction of principal/action/resource from Cedar text */
function extractPolicySummary(cedarText: string): string {
  const lines = cedarText.trim().split("\n").map((l) => l.trim());
  const parts: string[] = [];

  for (const line of lines) {
    // Principal
    const principalExact = line.match(/principal\s*==\s*(\w+)::"([^"]+)"/);
    if (principalExact) {
      parts.push(`${principalExact[1]}::${principalExact[2]}`);
      continue;
    }
    const principalIs = line.match(/principal\s+is\s+(\w+)/);
    if (principalIs) {
      parts.push(`any ${principalIs[1]}`);
      continue;
    }

    // Action
    const actionExact = line.match(/action\s*==\s*Action::"([^"]+)"/);
    if (actionExact) {
      parts.push(actionExact[1]);
      continue;
    }

    // Resource
    const resourceIs = line.match(/resource\s+is\s+(\w+)/);
    if (resourceIs) {
      parts.push(`on ${resourceIs[1]}`);
      continue;
    }
    const resourceExact = line.match(/resource\s*==\s*(\w+)::"([^"]+)"/);
    if (resourceExact) {
      parts.push(`on ${resourceExact[1]}::${resourceExact[2]}`);
      continue;
    }
  }

  return parts.length > 0 ? parts.join(" → ") : "";
}

export default function PolicyCard({
  policy,
  onToggle,
  onDelete,
  onUpdate,
  acting,
}: PolicyCardProps) {
  const [expanded, setExpanded] = useState(false);
  const [editing, setEditing] = useState(false);
  const [editText, setEditText] = useState(policy.cedar_text);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const relativeTime = useRelativeTime(policy.created_at);

  const badge = SOURCE_BADGES[policy.source] || SOURCE_BADGES.manual;
  const summary = useMemo(() => extractPolicySummary(policy.cedar_text), [policy.cedar_text]);
  const isLong = policy.cedar_text.split("\n").length > 5;

  const handleSave = () => {
    onUpdate(policy.policy_id, editText);
    setEditing(false);
  };

  const handleCancel = () => {
    setEditText(policy.cedar_text);
    setEditing(false);
  };

  return (
    <div
      className={`glass rounded p-4 animate-fade-in transition-opacity ${
        !policy.enabled ? "opacity-50" : ""
      }`}
    >
      {/* Header row */}
      <div className="flex items-start justify-between mb-2">
        <div className="flex items-center gap-2 min-w-0">
          <span
            className={`text-[10px] font-medium px-1.5 py-0.5 rounded-sm ${badge.className}`}
          >
            {badge.label}
          </span>
          <span
            className="text-[12px] font-mono text-[var(--color-text-secondary)] truncate"
            title={policy.policy_id}
          >
            {policy.policy_id}
          </span>
        </div>
        <div className="flex items-center gap-2 flex-shrink-0">
          {/* Enable/Disable toggle */}
          <button
            type="button"
            disabled={acting}
            onClick={() => onToggle(policy.policy_id, !policy.enabled)}
            className={`relative w-8 h-4 rounded-full transition-colors ${
              policy.enabled
                ? "bg-[var(--color-accent-teal)]"
                : "bg-[var(--color-text-muted)]"
            } ${acting ? "opacity-50" : ""}`}
            title={policy.enabled ? "Disable policy" : "Enable policy"}
            aria-label={policy.enabled ? "Disable policy" : "Enable policy"}
          >
            <span
              className={`absolute top-0.5 left-0.5 w-3 h-3 rounded-full bg-white transition-transform ${
                policy.enabled ? "translate-x-4" : "translate-x-0"
              }`}
            />
          </button>
        </div>
      </div>

      {/* Summary line */}
      {summary && (
        <div className="text-[12px] text-[var(--color-text-secondary)] mb-2 font-mono">
          {summary}
        </div>
      )}

      {/* Cedar text */}
      {editing ? (
        <div className="space-y-2">
          <textarea
            value={editText}
            onChange={(e) => setEditText(e.target.value)}
            className="w-full min-h-[120px] p-2.5 bg-black/30 rounded text-[11px] font-mono text-[var(--color-text-primary)] border border-[var(--color-border)] focus:outline-none focus:border-[var(--color-accent-teal)] resize-y"
            spellCheck={false}
          />
          <div className="flex gap-2">
            <button
              type="button"
              disabled={acting || editText === policy.cedar_text}
              onClick={handleSave}
              className="px-2.5 py-1 text-[11px] bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] rounded hover:bg-[var(--color-accent-teal-dim)] disabled:opacity-50 transition-colors"
            >
              Save
            </button>
            <button
              type="button"
              onClick={handleCancel}
              className="px-2.5 py-1 text-[11px] bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] rounded hover:bg-[var(--color-border)] transition-colors"
            >
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <div>
          <button
            type="button"
            onClick={() => setExpanded(!expanded)}
            className="w-full text-left"
          >
            <pre
              className={`p-2 bg-black/30 rounded text-[11px] font-mono text-[var(--color-text-secondary)] overflow-x-auto whitespace-pre-wrap border border-[var(--color-border)] ${
                !expanded && isLong ? "max-h-[80px] overflow-hidden" : ""
              }`}
            >
              {policy.cedar_text}
            </pre>
          </button>
          {isLong && (
            <button
              type="button"
              onClick={() => setExpanded(!expanded)}
              className="text-[10px] text-[var(--color-text-muted)] mt-1 hover:text-[var(--color-text-secondary)]"
            >
              {expanded ? "Collapse" : "Expand"}
            </button>
          )}
        </div>
      )}

      {/* Footer: metadata + actions */}
      <div className="flex items-center justify-between mt-2 pt-2 border-t border-[var(--color-border)]">
        <div className="flex items-center gap-3 text-[10px] text-[var(--color-text-muted)]">
          <span>by {policy.created_by}</span>
          {relativeTime && <span>{relativeTime}</span>}
          <span className="font-mono" title={policy.policy_hash}>
            {policy.policy_hash.slice(0, 8)}
          </span>
        </div>
        <div className="flex items-center gap-1.5">
          {!editing && (
            <button
              type="button"
              disabled={acting}
              onClick={() => {
                setEditText(policy.cedar_text);
                setEditing(true);
              }}
              className="px-2 py-0.5 text-[10px] text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-elevated)] rounded transition-colors disabled:opacity-50"
            >
              Edit
            </button>
          )}
          {confirmDelete ? (
            <div className="flex items-center gap-1">
              <span className="text-[10px] text-[var(--color-accent-pink)]">
                Delete?
              </span>
              <button
                type="button"
                disabled={acting}
                onClick={() => {
                  onDelete(policy.policy_id);
                  setConfirmDelete(false);
                }}
                className="px-2 py-0.5 text-[10px] bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] rounded disabled:opacity-50"
              >
                Yes
              </button>
              <button
                type="button"
                onClick={() => setConfirmDelete(false)}
                className="px-2 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)]"
              >
                No
              </button>
            </div>
          ) : (
            <button
              type="button"
              disabled={acting}
              onClick={() => setConfirmDelete(true)}
              className="px-2 py-0.5 text-[10px] text-[var(--color-text-muted)] hover:text-[var(--color-accent-pink)] hover:bg-[var(--color-accent-pink-dim)] rounded transition-colors disabled:opacity-50"
            >
              Delete
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
