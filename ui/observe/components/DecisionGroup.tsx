"use client";

import { useState } from "react";
import type { PendingDecision } from "@/lib/types";
import { groupLabel, type GroupingStrategy } from "@/lib/decision-grouping";
import { redactSensitiveFields } from "@/lib/utils";

interface DecisionGroupProps {
  groupKey: string;
  strategy: GroupingStrategy;
  decisions: PendingDecision[];
  selectedIds: Set<string>;
  onToggleSelect: (id: string) => void;
  onToggleGroup: (ids: string[]) => void;
}

function DecisionRow({
  decision,
  selected,
  onToggle,
}: {
  decision: PendingDecision;
  selected: boolean;
  onToggle: () => void;
}) {
  const redactedAttrs = redactSensitiveFields(decision.resource_attrs);
  const hasAttrs = decision.resource_attrs && Object.keys(decision.resource_attrs).length > 0;

  return (
    <div className="flex items-start gap-2.5 py-2 px-3 border-b border-[var(--color-border)] last:border-b-0">
      <input
        type="checkbox"
        checked={selected}
        onChange={onToggle}
        className="mt-1 accent-[var(--color-accent-teal)] flex-shrink-0"
      />
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 text-[12px]">
          <span className="font-mono text-[var(--color-text-secondary)] truncate max-w-[160px]" title={decision.agent_id}>
            {decision.agent_id}
          </span>
          <span className="text-[var(--color-text-muted)]">/</span>
          <span className="font-mono text-[var(--color-text-secondary)] truncate max-w-[200px]" title={`${decision.resource_type}::${decision.resource_id}`}>
            {decision.resource_type}::{decision.resource_id}
          </span>
        </div>
        <div className="text-[11px] text-[var(--color-accent-pink)] mt-0.5">
          {decision.denial_reason}
        </div>
        {hasAttrs && (
          <pre className="text-[10px] font-mono text-[var(--color-text-muted)] mt-0.5 truncate max-w-[400px]">
            {JSON.stringify(redactedAttrs)}
          </pre>
        )}
      </div>
      <span className="text-[10px] text-[var(--color-text-muted)] font-mono flex-shrink-0">
        {new Date(decision.created_at).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })}
      </span>
    </div>
  );
}

export default function DecisionGroup({
  groupKey: key,
  strategy,
  decisions,
  selectedIds,
  onToggleSelect,
  onToggleGroup,
}: DecisionGroupProps) {
  const [expanded, setExpanded] = useState(false);
  const ids = decisions.map((d) => d.id);
  const selectedCount = ids.filter((id) => selectedIds.has(id)).length;
  const allSelected = selectedCount === ids.length;
  const someSelected = selectedCount > 0 && !allSelected;
  const label = groupLabel(key, strategy);

  return (
    <div className="glass rounded animate-fade-in">
      {/* Group header */}
      <div className="flex items-center gap-2.5 p-3">
        <input
          type="checkbox"
          checked={allSelected}
          ref={(el) => {
            if (el) el.indeterminate = someSelected;
          }}
          onChange={() => onToggleGroup(ids)}
          className="accent-[var(--color-accent-teal)] flex-shrink-0"
        />
        <button
          type="button"
          onClick={() => setExpanded(!expanded)}
          className="flex items-center gap-2 flex-1 min-w-0 text-left"
        >
          <span className="text-sm font-mono text-[var(--color-text-primary)] truncate">
            {label}
          </span>
          <span className="text-[10px] font-mono px-1.5 py-0.5 rounded-full bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]">
            {decisions.length}
          </span>
          {selectedCount > 0 && selectedCount < decisions.length && (
            <span className="text-[10px] text-[var(--color-text-muted)]">
              {selectedCount} selected
            </span>
          )}
          <span className="ml-auto text-[var(--color-text-muted)] text-[10px]">
            {expanded ? "\u25B4" : "\u25BE"}
          </span>
        </button>
      </div>

      {/* Expanded decision list */}
      {expanded && (
        <div className="border-t border-[var(--color-border)]">
          {decisions.map((d) => (
            <DecisionRow
              key={d.id}
              decision={d}
              selected={selectedIds.has(d.id)}
              onToggle={() => onToggleSelect(d.id)}
            />
          ))}
        </div>
      )}
    </div>
  );
}
