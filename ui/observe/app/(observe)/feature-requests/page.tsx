"use client";

import { useState, useCallback, useMemo } from "react";
import {
  fetchFeatureRequests,
  updateFeatureRequest,
} from "@/lib/api";
import { usePolling, useRelativeTime } from "@/lib/hooks";
import type {
  FeatureRequest,
  FeatureRequestDisposition,
  PlatformGapCategory,
} from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

const categoryColors: Record<PlatformGapCategory, string> = {
  MissingMethod: "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  GovernanceBlocked: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  UnsupportedIntegration: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  MissingCapability: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
};

const dispositionColors: Record<FeatureRequestDisposition, string> = {
  Open: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  Acknowledged: "bg-blue-500/15 text-blue-400",
  Planned: "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  WontFix: "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]",
  Resolved: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
};

const DISPOSITIONS: FeatureRequestDisposition[] = [
  "Open",
  "Acknowledged",
  "Planned",
  "WontFix",
  "Resolved",
];

type FilterTab = "all" | FeatureRequestDisposition;

function FeatureRequestCard({
  request,
  onUpdate,
  acting,
}: {
  request: FeatureRequest;
  onUpdate: (id: string, disposition: FeatureRequestDisposition, notes?: string) => void;
  acting: boolean;
}) {
  const [showNotes, setShowNotes] = useState(false);
  const [notes, setNotes] = useState(request.developer_notes ?? "");
  const createdAt = new Date(request.created_at).toLocaleString();

  return (
    <div className="bg-[var(--color-bg-surface)] rounded-[2px] p-4 animate-fade-in">
      {/* Header row */}
      <div className="flex items-start justify-between mb-3">
        <div className="flex items-center gap-2 flex-wrap">
          <span
            className={`text-xs font-medium px-1.5 py-0.5 rounded ${
              categoryColors[request.category] ?? "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"
            }`}
          >
            {request.category}
          </span>
          <span
            className={`text-xs font-medium px-1.5 py-0.5 rounded ${
              dispositionColors[request.disposition] ?? "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"
            }`}
          >
            {request.disposition}
          </span>
          <span className="text-xs font-mono text-[var(--color-text-muted)]">
            {request.id.slice(0, 12)}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-xs font-mono text-[var(--color-text-secondary)]">
            {request.frequency}x
          </span>
        </div>
      </div>

      {/* Description */}
      <p className="text-sm text-[var(--color-text-secondary)] mb-3">{request.description}</p>

      {/* Trajectory refs */}
      {request.trajectory_refs.length > 0 && (
        <div className="flex items-center gap-2 mb-3">
          <span className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider">
            Trajectories
          </span>
          <div className="flex flex-wrap gap-1">
            {request.trajectory_refs.slice(0, 5).map((ref) => (
              <span
                key={ref}
                className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)]"
              >
                {ref.slice(0, 12)}
              </span>
            ))}
            {request.trajectory_refs.length > 5 && (
              <span className="text-[10px] font-mono text-[var(--color-text-muted)]">
                +{request.trajectory_refs.length - 5} more
              </span>
            )}
          </div>
        </div>
      )}

      {/* Developer notes */}
      {request.developer_notes && !showNotes && (
        <div className="mb-3 px-3 py-2 bg-black/20 rounded text-xs text-[var(--color-text-secondary)]">
          {request.developer_notes}
        </div>
      )}

      {/* Timestamp */}
      <div className="text-[10px] text-[var(--color-text-muted)] mb-3 font-mono">{createdAt}</div>

      {/* Actions */}
      <div className="flex items-center gap-2 pt-2 border-t border-[var(--color-border)]">
        {DISPOSITIONS.filter((d) => d !== request.disposition).map((d) => (
          <button
            key={d}
            onClick={() => onUpdate(request.id, d, showNotes ? notes : undefined)}
            disabled={acting}
            className={`px-2.5 py-1 text-[11px] rounded-sm transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
              d === "Resolved"
                ? "bg-[var(--color-accent-teal-dim)] hover:bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]"
                : d === "WontFix"
                  ? "bg-[var(--color-accent-lime-dim)] hover:bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"
                  : d === "Planned"
                    ? "bg-[var(--color-accent-lime-dim)] hover:bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]"
                    : d === "Acknowledged"
                      ? "bg-blue-500/20 hover:bg-blue-500/30 text-blue-400"
                      : "bg-[var(--color-accent-pink-dim)] hover:bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"
            }`}
          >
            {d === "WontFix" ? "Won't Fix" : d}
          </button>
        ))}
        <button
          onClick={() => setShowNotes(!showNotes)}
          className="ml-auto text-[11px] text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] transition-colors"
        >
          {showNotes ? "Hide Notes" : "Add Notes"}
        </button>
      </div>

      {/* Notes input */}
      {showNotes && (
        <div className="mt-3 flex gap-2">
          <input
            type="text"
            value={notes}
            onChange={(e) => setNotes(e.target.value)}
            placeholder="Developer notes..."
            className="flex-1 bg-black/30 border border-[var(--color-border)] rounded-sm px-2.5 py-1.5 text-xs text-[var(--color-text-secondary)] placeholder-[var(--color-text-muted)] focus:outline-none focus:border-[var(--color-accent-teal)]/30"
          />
          <button
            onClick={() => {
              onUpdate(request.id, request.disposition, notes);
              setShowNotes(false);
            }}
            disabled={acting}
            className="px-2.5 py-1.5 bg-[var(--color-accent-teal-dim)] hover:bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] text-xs rounded-sm transition-colors disabled:opacity-40"
          >
            Save
          </button>
        </div>
      )}
    </div>
  );
}

export default function FeatureRequestsPage() {
  const [activeTab, setActiveTab] = useState<FilterTab>("all");
  const [acting, setActing] = useState(false);

  const featuresPoll = usePolling<FeatureRequest[]>({
    fetcher: fetchFeatureRequests,
    interval: 10000,
  });

  const lastUpdated = useRelativeTime(featuresPoll.lastUpdated);

  const handleUpdate = useCallback(
    async (id: string, disposition: FeatureRequestDisposition, notes?: string) => {
      setActing(true);
      try {
        const update: { disposition?: FeatureRequestDisposition; developer_notes?: string } = {
          disposition,
        };
        if (notes !== undefined) {
          update.developer_notes = notes;
        }
        const ok = await updateFeatureRequest(id, update);
        if (ok) {
          await featuresPoll.refresh();
        }
      } catch {
        // Silently handled by polling
      } finally {
        setActing(false);
      }
    },
    [featuresPoll],
  );

  const requests = featuresPoll.data;

  const filteredRequests = useMemo(() => {
    if (!requests) return [];
    if (activeTab === "all") return requests;
    return requests.filter((r) => r.disposition === activeTab);
  }, [requests, activeTab]);

  const counts = useMemo(() => {
    if (!requests) return { total: 0, open: 0, acknowledged: 0, planned: 0, wontfix: 0, resolved: 0 };
    return {
      total: requests.length,
      open: requests.filter((r) => r.disposition === "Open").length,
      acknowledged: requests.filter((r) => r.disposition === "Acknowledged").length,
      planned: requests.filter((r) => r.disposition === "Planned").length,
      wontfix: requests.filter((r) => r.disposition === "WontFix").length,
      resolved: requests.filter((r) => r.disposition === "Resolved").length,
    };
  }, [requests]);

  if (featuresPoll.loading && !requests) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-[var(--color-border)] rounded w-48 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-72 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (featuresPoll.error && !requests) {
    return (
      <ErrorDisplay
        title="Cannot load feature requests"
        message={featuresPoll.error}
        retry={() => featuresPoll.refresh()}
      />
    );
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">
            Feature Requests
          </h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            Platform gaps detected from trajectory analysis and agent feedback
          </p>
        </div>
        <div className="flex items-center gap-3">
          {counts.open > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full" />
              <span className="text-xs text-[var(--color-accent-pink)]">
                {counts.open} open
              </span>
            </div>
          )}
          {lastUpdated && (
            <span className="text-xs text-[var(--color-text-muted)]">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Summary Cards */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <div className="glass rounded-[2px] p-4">
          <div className="text-xs text-[var(--color-text-muted)]">Open</div>
          <div className={`text-4xl font-bold font-mono mt-0.5 ${counts.open > 0 ? "text-[var(--color-accent-pink)]" : "text-[var(--color-text-primary)]"}`}>
            {counts.open}
          </div>
        </div>
        <div className="glass rounded-[2px] p-4">
          <div className="text-xs text-[var(--color-text-muted)]">Planned</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-[var(--color-accent-lime)]">
            {counts.planned}
          </div>
        </div>
        <div className="glass rounded-[2px] p-4">
          <div className="text-xs text-[var(--color-text-muted)]">Resolved</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-[var(--color-accent-teal)]">
            {counts.resolved}
          </div>
        </div>
        <div className="glass rounded-[2px] p-4">
          <div className="text-xs text-[var(--color-text-muted)]">Total</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-[var(--color-text-primary)]">
            {counts.total}
          </div>
        </div>
      </div>

      {/* Filter Tabs */}
      <div className="flex items-center gap-4 mb-4">
        <div className="flex gap-1">
          {(["all", ...DISPOSITIONS] as FilterTab[]).map((tab) => (
            <button
              key={tab}
              onClick={() => setActiveTab(tab)}
              className={`text-xs px-2 py-1 rounded transition-colors ${
                activeTab === tab
                  ? "text-[var(--color-accent-teal)] border-b-2 border-[var(--color-accent-teal)]"
                  : "text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)]"
              }`}
            >
              {tab === "all" ? "All" : tab === "WontFix" ? "Won't Fix" : tab}
            </button>
          ))}
        </div>
      </div>

      {/* Request Cards */}
      {filteredRequests.length === 0 ? (
        <div className="glass rounded-[2px] p-6 text-center">
          <p className="text-sm text-[var(--color-text-secondary)]">
            {activeTab === "all"
              ? "No feature requests yet. They will appear as platform gaps are detected."
              : `No feature requests with disposition "${activeTab === "WontFix" ? "Won't Fix" : activeTab}".`}
          </p>
        </div>
      ) : (
        <div className="grid gap-3">
          {filteredRequests
            .sort((a, b) => b.frequency - a.frequency)
            .map((request) => (
              <FeatureRequestCard
                key={request.id}
                request={request}
                onUpdate={handleUpdate}
                acting={acting}
              />
            ))}
        </div>
      )}
    </div>
  );
}
