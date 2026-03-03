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
  MissingMethod: "bg-violet-500/15 text-violet-300",
  GovernanceBlocked: "bg-pink-500/15 text-pink-400",
  UnsupportedIntegration: "bg-yellow-500/15 text-yellow-400",
  MissingCapability: "bg-teal-500/15 text-teal-400",
};

const dispositionColors: Record<FeatureRequestDisposition, string> = {
  Open: "bg-yellow-500/15 text-yellow-400",
  Acknowledged: "bg-blue-500/15 text-blue-400",
  Planned: "bg-lime-500/15 text-lime-400",
  WontFix: "bg-zinc-500/15 text-zinc-500",
  Resolved: "bg-teal-500/15 text-teal-400",
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
    <div className="bg-[#111115] rounded-lg p-4 animate-fade-in">
      {/* Header row */}
      <div className="flex items-start justify-between mb-3">
        <div className="flex items-center gap-2 flex-wrap">
          <span
            className={`text-xs font-medium px-1.5 py-0.5 rounded ${
              categoryColors[request.category] ?? "bg-zinc-500/15 text-zinc-400"
            }`}
          >
            {request.category}
          </span>
          <span
            className={`text-xs font-medium px-1.5 py-0.5 rounded ${
              dispositionColors[request.disposition] ?? "bg-zinc-500/15 text-zinc-400"
            }`}
          >
            {request.disposition}
          </span>
          <span className="text-xs font-mono text-zinc-700">
            {request.id.slice(0, 12)}
          </span>
        </div>
        <div className="flex items-center gap-2">
          <span className="text-xs font-mono text-zinc-500">
            {request.frequency}x
          </span>
        </div>
      </div>

      {/* Description */}
      <p className="text-sm text-zinc-300 mb-3">{request.description}</p>

      {/* Trajectory refs */}
      {request.trajectory_refs.length > 0 && (
        <div className="flex items-center gap-2 mb-3">
          <span className="text-[10px] text-zinc-600 uppercase tracking-wider">
            Trajectories
          </span>
          <div className="flex flex-wrap gap-1">
            {request.trajectory_refs.slice(0, 5).map((ref) => (
              <span
                key={ref}
                className="text-[10px] font-mono px-1.5 py-0.5 rounded bg-zinc-800 text-zinc-500"
              >
                {ref.slice(0, 12)}
              </span>
            ))}
            {request.trajectory_refs.length > 5 && (
              <span className="text-[10px] font-mono text-zinc-600">
                +{request.trajectory_refs.length - 5} more
              </span>
            )}
          </div>
        </div>
      )}

      {/* Developer notes */}
      {request.developer_notes && !showNotes && (
        <div className="mb-3 px-3 py-2 bg-black/20 rounded text-xs text-zinc-400">
          {request.developer_notes}
        </div>
      )}

      {/* Timestamp */}
      <div className="text-[10px] text-zinc-700 mb-3 font-mono">{createdAt}</div>

      {/* Actions */}
      <div className="flex items-center gap-2 pt-2 border-t border-white/[0.04]">
        {DISPOSITIONS.filter((d) => d !== request.disposition).map((d) => (
          <button
            key={d}
            onClick={() => onUpdate(request.id, d, showNotes ? notes : undefined)}
            disabled={acting}
            className={`px-2.5 py-1 text-[11px] rounded-sm transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
              d === "Resolved"
                ? "bg-teal-500/20 hover:bg-teal-500/30 text-teal-400"
                : d === "WontFix"
                  ? "bg-zinc-500/20 hover:bg-zinc-500/30 text-zinc-400"
                  : d === "Planned"
                    ? "bg-lime-500/20 hover:bg-lime-500/30 text-lime-400"
                    : d === "Acknowledged"
                      ? "bg-blue-500/20 hover:bg-blue-500/30 text-blue-400"
                      : "bg-yellow-500/20 hover:bg-yellow-500/30 text-yellow-400"
            }`}
          >
            {d === "WontFix" ? "Won't Fix" : d}
          </button>
        ))}
        <button
          onClick={() => setShowNotes(!showNotes)}
          className="ml-auto text-[11px] text-zinc-600 hover:text-zinc-400 transition-colors"
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
            className="flex-1 bg-black/30 border border-white/[0.06] rounded-sm px-2.5 py-1.5 text-xs text-zinc-300 placeholder-zinc-700 focus:outline-none focus:border-teal-500/30"
          />
          <button
            onClick={() => {
              onUpdate(request.id, request.disposition, notes);
              setShowNotes(false);
            }}
            disabled={acting}
            className="px-2.5 py-1.5 bg-teal-500/20 hover:bg-teal-500/30 text-teal-400 text-xs rounded-sm transition-colors disabled:opacity-40"
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
        <div className="h-6 bg-zinc-800/60 rounded w-48 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-72 mb-6" />
        <div className="grid grid-cols-4 gap-3 mb-6">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="bg-[#111115] rounded-lg p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
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
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">
            Feature Requests
          </h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            Platform gaps detected from trajectory analysis and agent feedback
          </p>
        </div>
        <div className="flex items-center gap-3">
          {counts.open > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-2 h-2 bg-yellow-400 rounded-full" />
              <span className="text-xs text-yellow-400">
                {counts.open} open
              </span>
            </div>
          )}
          {lastUpdated && (
            <span className="text-xs text-zinc-600">Updated {lastUpdated}</span>
          )}
        </div>
      </div>

      {/* Summary Cards */}
      <div className="grid grid-cols-4 gap-3 mb-6">
        <div className="bg-[#111115] rounded-lg p-3.5">
          <div className="text-xs text-zinc-600">Open</div>
          <div className={`text-4xl font-bold font-mono mt-0.5 ${counts.open > 0 ? "text-yellow-400" : "text-zinc-100"}`}>
            {counts.open}
          </div>
        </div>
        <div className="bg-[#111115] rounded-lg p-3.5">
          <div className="text-xs text-zinc-600">Planned</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-lime-400">
            {counts.planned}
          </div>
        </div>
        <div className="bg-[#111115] rounded-lg p-3.5">
          <div className="text-xs text-zinc-600">Resolved</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-teal-400">
            {counts.resolved}
          </div>
        </div>
        <div className="bg-[#111115] rounded-lg p-3.5">
          <div className="text-xs text-zinc-600">Total</div>
          <div className="text-4xl font-bold font-mono mt-0.5 text-zinc-100">
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
                  ? "text-teal-400 border-b-2 border-teal-400"
                  : "text-zinc-600 hover:text-zinc-400"
              }`}
            >
              {tab === "all" ? "All" : tab === "WontFix" ? "Won't Fix" : tab}
            </button>
          ))}
        </div>
      </div>

      {/* Request Cards */}
      {filteredRequests.length === 0 ? (
        <div className="bg-[#111115] rounded-lg p-6 text-center">
          <p className="text-sm text-zinc-500">
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
