"use client";

import { useState, useCallback, useEffect } from "react";
import {
  fetchEvolutionRecords,
  fetchEvolutionInsights,
  triggerSentinelCheck,
} from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type {
  EvolutionRecordsResponse,
  EvolutionInsightsResponse,
  SentinelCheckResponse,
} from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

const recordTypeColors: Record<string, string> = {
  Observation: "bg-teal-500/15 text-teal-400",
  Problem: "bg-pink-500/15 text-pink-400",
  Analysis: "bg-violet-500/15 text-violet-300",
  Decision: "bg-lime-500/15 text-lime-400",
  Insight: "bg-yellow-500/15 text-yellow-400",
};

function priorityBadge(score: number): { bg: string; label: string } {
  if (score > 0.7) return { bg: "bg-pink-500/15 text-pink-400", label: "high" };
  if (score >= 0.3) return { bg: "bg-yellow-500/15 text-yellow-400", label: "medium" };
  return { bg: "bg-zinc-500/15 text-zinc-400", label: "low" };
}

function TrendArrow({ trend }: { trend: string }) {
  if (trend === "growing") {
    return <span className="text-lime-400 text-xs" aria-label="growing trend">&#9650;</span>;
  }
  if (trend === "declining") {
    return <span className="text-pink-400 text-xs" aria-label="declining trend">&#9660;</span>;
  }
  return <span className="text-zinc-600 text-xs" aria-label="stable trend">&#8212;</span>;
}

export default function EvolutionPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [sentinelResult, setSentinelResult] = useState<SentinelCheckResponse | null>(null);
  const [sentinelLoading, setSentinelLoading] = useState(false);
  const [activeTab, setActiveTab] = useState<"all" | "observations" | "insights">("all");

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await fetchEvolutionRecords();
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load evolution data");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  const recordsPoll = usePolling<EvolutionRecordsResponse>({
    fetcher: fetchEvolutionRecords,
    interval: 10000,
    enabled: !initialLoading && !initialError,
  });

  const insightsPoll = usePolling<EvolutionInsightsResponse>({
    fetcher: fetchEvolutionInsights,
    interval: 10000,
    enabled: !initialLoading && !initialError,
  });

  const records = recordsPoll.data;
  const insights = insightsPoll.data;

  const handleSentinelCheck = useCallback(async () => {
    setSentinelLoading(true);
    try {
      const result = await triggerSentinelCheck();
      setSentinelResult(result);
    } catch {
      setSentinelResult(null);
    } finally {
      setSentinelLoading(false);
    }
  }, []);

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-zinc-800/60 rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-zinc-800/40 rounded w-64 mb-6" />
        <div className="grid grid-cols-5 gap-3 mb-6">
          {[0, 1, 2, 3, 4].map((i) => (
            <div key={i} className="bg-[#111115] rounded-lg p-3.5">
              <div className="h-3 bg-zinc-800/50 rounded w-20 mb-2" />
              <div className="h-8 bg-zinc-800/50 rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (initialError) {
    return <ErrorDisplay title="Cannot load evolution data" message={initialError} retry={loadInitial} />;
  }

  const summaryCards = [
    { label: "Observations", value: records?.total_observations ?? 0, color: "text-teal-400" },
    { label: "Problems", value: records?.total_problems ?? 0, color: "text-pink-400" },
    { label: "Analyses", value: records?.total_analyses ?? 0, color: "text-violet-300" },
    { label: "Decisions", value: records?.total_decisions ?? 0, color: "text-lime-400" },
    { label: "Insights", value: records?.total_insights ?? 0, color: "text-yellow-400" },
  ];

  const filteredRecords = records?.records.filter((r) => {
    if (activeTab === "observations") return r.record_type === "Observation";
    if (activeTab === "insights") return r.record_type === "Insight";
    return true;
  }) ?? [];

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">Evolution</h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            O-P-A-D-I record chain, insights, and sentinel health
          </p>
        </div>
        <div className="flex items-center gap-3">
          {sentinelResult && (
            <div className="flex items-center gap-1.5">
              {sentinelResult.alerts_count === 0 ? (
                <>
                  <svg className="w-3.5 h-3.5 text-teal-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                  </svg>
                  <span className="text-xs text-teal-400">Healthy</span>
                </>
              ) : (
                <>
                  <div className="w-2 h-2 bg-pink-400 rounded-full animate-pulse" />
                  <span className="text-xs text-pink-400">{sentinelResult.alerts_count} alert{sentinelResult.alerts_count !== 1 ? "s" : ""}</span>
                </>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Record Summary */}
      <div className="grid grid-cols-5 gap-3 mb-6">
        {summaryCards.map((card) => (
          <div key={card.label} className="bg-[#111115] rounded-lg p-3.5">
            <div className="text-xs text-zinc-600">{card.label}</div>
            <div className={`text-4xl font-bold font-mono mt-0.5 ${card.color}`}>
              {card.value}
            </div>
          </div>
        ))}
      </div>

      {/* Insights Panel */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-zinc-200 mb-3 tracking-tight">Insights</h2>
        {!insights || insights.insights.length === 0 ? (
          <div className="bg-[#111115] rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">No insights yet. Insights appear as the system observes patterns.</p>
          </div>
        ) : (
          <div className="space-y-2">
            {insights.insights
              .sort((a, b) => b.priority_score - a.priority_score)
              .map((insight) => {
                const badge = priorityBadge(insight.priority_score);
                return (
                  <div
                    key={insight.id}
                    className="bg-[#111115] rounded-lg p-3.5"
                  >
                    <div className="flex items-center gap-2 mb-2">
                      <span className={`text-xs font-medium px-1.5 py-0.5 rounded ${recordTypeColors.Insight ?? "bg-yellow-500/15 text-yellow-400"}`}>
                        {insight.category}
                      </span>
                      <span className={`text-xs font-medium px-1.5 py-0.5 rounded ${badge.bg}`}>
                        {badge.label}
                      </span>
                      <span className="text-xs font-mono text-zinc-700 ml-auto">
                        {insight.priority_score.toFixed(2)}
                      </span>
                    </div>
                    <p className="text-sm text-zinc-300 mb-2">{insight.recommendation}</p>
                    <div className="flex items-center gap-4 text-xs text-zinc-600">
                      <span>Intent: <span className="text-zinc-400 font-mono">{insight.signal.intent}</span></span>
                      <span>Vol: <span className="text-zinc-400 font-mono">{insight.signal.volume}</span></span>
                      <span>Rate: <span className="text-zinc-400 font-mono">{Math.round(insight.signal.success_rate * 100)}%</span></span>
                      <span className="flex items-center gap-1">
                        Trend: <TrendArrow trend={insight.signal.trend} />
                      </span>
                    </div>
                  </div>
                );
              })}
          </div>
        )}
      </div>

      {/* Records with tab filter */}
      <div className="mb-6">
        <div className="flex items-center gap-4 mb-3">
          <h2 className="text-base font-semibold text-zinc-200 tracking-tight">Records</h2>
          <div className="flex gap-1">
            {(["all", "observations", "insights"] as const).map((tab) => (
              <button
                key={tab}
                onClick={() => setActiveTab(tab)}
                className={`text-xs px-2 py-1 rounded transition-colors ${
                  activeTab === tab
                    ? "text-teal-400 border-b-2 border-teal-400"
                    : "text-zinc-600 hover:text-zinc-400"
                }`}
              >
                {tab.charAt(0).toUpperCase() + tab.slice(1)}
              </button>
            ))}
          </div>
        </div>
        {filteredRecords.length === 0 ? (
          <div className="bg-[#111115] rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">No records found.</p>
          </div>
        ) : (
          <div className="bg-[#111115] rounded-lg overflow-hidden max-h-80 overflow-y-auto">
            {filteredRecords.map((record) => (
              <div
                key={record.id}
                className="flex items-center gap-3 px-3.5 py-2.5 border-b border-white/[0.03] last:border-b-0"
              >
                <span className={`text-xs font-medium px-1.5 py-0.5 rounded flex-shrink-0 ${recordTypeColors[record.record_type] ?? "bg-zinc-500/15 text-zinc-400"}`}>
                  {record.record_type}
                </span>
                <span className="text-xs text-zinc-500 font-mono flex-shrink-0">{record.id.slice(0, 8)}</span>
                {record.source && (
                  <span className="text-xs text-zinc-400 truncate">{record.source}</span>
                )}
                {record.recommendation && (
                  <span className="text-xs text-zinc-400 truncate">{record.recommendation}</span>
                )}
                <span className={`text-xs px-1.5 py-0.5 rounded ml-auto flex-shrink-0 ${
                  record.status === "active" ? "bg-teal-500/15 text-teal-400" : "bg-zinc-500/15 text-zinc-500"
                }`}>
                  {record.status}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Sentinel Panel */}
      <div>
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-base font-semibold text-zinc-200 tracking-tight">Sentinel Health Check</h2>
          <button
            onClick={handleSentinelCheck}
            disabled={sentinelLoading}
            className="px-3 py-1.5 bg-teal-500 hover:bg-teal-400 disabled:bg-teal-500/50 text-white text-xs rounded-md transition-colors"
          >
            {sentinelLoading ? "Checking..." : "Run Health Check"}
          </button>
        </div>
        {sentinelResult === null ? (
          <div className="bg-[#111115] rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">Click &quot;Run Health Check&quot; to check system health.</p>
          </div>
        ) : sentinelResult.alerts_count === 0 ? (
          <div className="bg-teal-500/5 rounded-lg p-4 flex items-center gap-3">
            <svg className="w-5 h-5 text-teal-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
            </svg>
            <span className="text-sm text-teal-400">All checks passed. No alerts.</span>
          </div>
        ) : (
          <div className="space-y-2">
            {sentinelResult.alerts.map((alert, i) => (
              <div
                key={`${alert.rule}-${i}`}
                className="bg-pink-500/5 rounded-lg p-3.5 animate-pulse-once"
              >
                <div className="flex items-center gap-2 mb-1.5">
                  <div className="w-2 h-2 bg-pink-400 rounded-full animate-pulse" />
                  <span className="text-sm font-medium text-pink-300">{alert.rule}</span>
                  <span className={`text-[10px] px-1.5 py-0.5 rounded ml-auto ${recordTypeColors[alert.classification] ?? "bg-zinc-500/15 text-zinc-400"}`}>
                    {alert.classification}
                  </span>
                </div>
                <div className="flex items-center gap-4 text-xs text-zinc-600">
                  <span>Source: <span className="text-zinc-400 font-mono">{alert.source}</span></span>
                  <span>Threshold: <span className="text-zinc-400 font-mono">{alert.threshold}</span></span>
                  <span>Observed: <span className="text-pink-400 font-mono">{alert.observed}</span></span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
