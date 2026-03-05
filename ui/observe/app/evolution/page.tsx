"use client";

import { useState, useCallback, useEffect } from "react";
import {
  fetchEvolutionRecords,
  fetchEvolutionInsights,
  triggerExtendedSentinelCheck,
  fetchUnmetIntents,
  fetchRecordDetail,
  subscribeEvolutionEvents,
} from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type {
  EvolutionRecordsResponse,
  EvolutionInsightsResponse,
  ExtendedSentinelCheckResponse,
  UnmetIntentsResponse,
  EvolutionRecord,
  EvolutionRecordDetail,
} from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";

const recordTypeColors: Record<string, string> = {
  Observation: "bg-teal-500/15 text-teal-400",
  Problem: "bg-pink-500/15 text-pink-400",
  Analysis: "bg-violet-500/15 text-violet-300",
  Decision: "bg-lime-500/15 text-lime-400",
  Insight: "bg-yellow-500/15 text-yellow-400",
};

const errorPatternColors: Record<string, string> = {
  EntitySetNotFound: "bg-yellow-500/15 text-yellow-400",
  AuthzDenied: "bg-pink-500/15 text-pink-400",
  ActionNotFound: "bg-violet-500/15 text-violet-300",
  GuardRejected: "bg-zinc-500/15 text-zinc-400",
  Other: "bg-zinc-500/15 text-zinc-500",
};

type RecordTab = "all" | "observations" | "problems" | "analyses" | "decisions" | "insights";

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

function RecordDetail({ detail }: { detail: EvolutionRecordDetail }) {
  return (
    <div className="px-4 py-3 bg-zinc-900/50 border-t border-white/[0.03]">
      {detail.derived_from && (
        <div className="text-xs text-zinc-500 mb-2">
          Derived from: <span className="font-mono text-zinc-400">{detail.derived_from}</span>
        </div>
      )}
      {/* O-Record fields */}
      {detail.evidence_query && (
        <div className="mb-2">
          <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1">Evidence</div>
          <pre className="text-xs text-zinc-300 font-mono bg-black/30 rounded p-2 overflow-x-auto">{detail.evidence_query}</pre>
        </div>
      )}
      {detail.threshold_field && (
        <div className="flex gap-4 text-xs text-zinc-500 mb-2">
          <span>Field: <span className="text-zinc-300 font-mono">{detail.threshold_field}</span></span>
          {detail.threshold_value != null && <span>Threshold: <span className="text-zinc-300 font-mono">{detail.threshold_value}</span></span>}
          {detail.observed_value != null && <span>Observed: <span className="text-pink-400 font-mono">{detail.observed_value}</span></span>}
        </div>
      )}
      {/* A-Record fields */}
      {detail.root_cause && (
        <div className="mb-2">
          <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1">Root Cause</div>
          <p className="text-xs text-zinc-300">{detail.root_cause}</p>
        </div>
      )}
      {detail.options && detail.options.length > 0 && (
        <div className="mb-2">
          <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1">Options</div>
          {detail.options.map((opt, i) => (
            <div key={i} className="bg-black/20 rounded p-2 mb-1 text-xs">
              <p className="text-zinc-300">{opt.description}</p>
              {opt.spec_diff && (
                <pre className="text-[10px] text-zinc-500 font-mono mt-1 max-h-32 overflow-y-auto">{opt.spec_diff}</pre>
              )}
              <div className="flex gap-3 mt-1 text-zinc-600">
                <span>Risk: {opt.risk}</span>
                <span>Complexity: {opt.complexity}</span>
              </div>
            </div>
          ))}
        </div>
      )}
      {/* D-Record fields */}
      {detail.decision && (
        <div className="mb-2">
          <div className="flex items-center gap-2">
            <span className={`text-xs px-1.5 py-0.5 rounded ${
              detail.decision === "Approved" ? "bg-lime-500/15 text-lime-400" :
              detail.decision === "Rejected" ? "bg-pink-500/15 text-pink-400" :
              "bg-yellow-500/15 text-yellow-400"
            }`}>{detail.decision}</span>
            {detail.decided_by && <span className="text-xs text-zinc-500">by {detail.decided_by}</span>}
          </div>
          {detail.rationale && <p className="text-xs text-zinc-400 mt-1">{detail.rationale}</p>}
        </div>
      )}
      {/* I-Record fields */}
      {detail.signal && (
        <div className="flex gap-4 text-xs text-zinc-500">
          <span>Intent: <span className="text-zinc-300 font-mono">{detail.signal.intent}</span></span>
          <span>Volume: <span className="text-zinc-300 font-mono">{detail.signal.volume}</span></span>
          <span>Rate: <span className="text-zinc-300 font-mono">{Math.round(detail.signal.success_rate * 100)}%</span></span>
        </div>
      )}
    </div>
  );
}

function ExpandableRecord({ record }: { record: EvolutionRecord }) {
  const [expanded, setExpanded] = useState(false);
  const [detail, setDetail] = useState<EvolutionRecordDetail | null>(null);
  const [loading, setLoading] = useState(false);

  const handleExpand = async () => {
    if (expanded) {
      setExpanded(false);
      return;
    }
    setExpanded(true);
    if (!detail) {
      setLoading(true);
      try {
        const d = await fetchRecordDetail(record.id);
        setDetail(d);
      } catch {
        setDetail(null);
      } finally {
        setLoading(false);
      }
    }
  };

  return (
    <div>
      <button
        onClick={handleExpand}
        className="w-full flex items-center gap-3 px-3.5 py-2.5 border-b border-white/[0.03] last:border-b-0 hover:bg-white/[0.02] transition-colors text-left"
      >
        <span className="text-[10px] text-zinc-700 flex-shrink-0">{expanded ? "▼" : "▶"}</span>
        <span className={`text-xs font-medium px-1.5 py-0.5 rounded flex-shrink-0 ${recordTypeColors[record.record_type] ?? "bg-zinc-500/15 text-zinc-400"}`}>
          {record.record_type}
        </span>
        <span className="text-xs text-zinc-500 font-mono flex-shrink-0">{record.id?.slice(0, 12) ?? "—"}</span>
        {record.source && (
          <span className="text-xs text-zinc-400 truncate">{record.source}</span>
        )}
        {record.recommendation && (
          <span className="text-xs text-zinc-400 truncate">{record.recommendation}</span>
        )}
        <span className={`text-xs px-1.5 py-0.5 rounded ml-auto flex-shrink-0 ${
          record.status === "active" || record.status === "Open" ? "bg-teal-500/15 text-teal-400" : "bg-zinc-500/15 text-zinc-500"
        }`}>
          {record.status}
        </span>
      </button>
      {expanded && (
        loading ? (
          <div className="px-4 py-3 bg-zinc-900/50 border-t border-white/[0.03]">
            <span className="text-xs text-zinc-600 animate-pulse">Loading...</span>
          </div>
        ) : detail ? (
          <RecordDetail detail={detail} />
        ) : null
      )}
    </div>
  );
}

export default function EvolutionPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [sentinelResult, setSentinelResult] = useState<ExtendedSentinelCheckResponse | null>(null);
  const [sentinelLoading, setSentinelLoading] = useState(false);
  const [activeTab, setActiveTab] = useState<RecordTab>("all");

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

  // Subscribe to SSE for real-time updates
  useEffect(() => {
    const cleanup = subscribeEvolutionEvents(() => {
      // Trigger a refresh on any evolution event
      recordsPoll.refresh();
      insightsPoll.refresh();
      unmetPoll.refresh();
    });
    return cleanup;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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

  const unmetPoll = usePolling<UnmetIntentsResponse>({
    fetcher: fetchUnmetIntents,
    interval: 10000,
    enabled: !initialLoading && !initialError,
  });

  const records = recordsPoll.data;
  const insights = insightsPoll.data;
  const unmetIntents = unmetPoll.data;

  const handleSentinelCheck = useCallback(async () => {
    setSentinelLoading(true);
    try {
      const result = await triggerExtendedSentinelCheck();
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
            <div key={i} className="glass rounded-lg p-4">
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

  const tabToType: Record<RecordTab, string | null> = {
    all: null,
    observations: "Observation",
    problems: "Problem",
    analyses: "Analysis",
    decisions: "Decision",
    insights: "Insight",
  };

  const filteredRecords = records?.records.filter((r) => {
    const typeFilter = tabToType[activeTab];
    return !typeFilter || r.record_type === typeFilter;
  }) ?? [];

  const openIntentCount = unmetIntents?.open_count ?? 0;

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">Evolution</h1>
          <p className="text-sm text-zinc-600 mt-0.5">
            O-P-A-D-I record chain, insights, unmet intents, and sentinel health
          </p>
        </div>
        <div className="flex items-center gap-3">
          {openIntentCount > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-2 h-2 bg-yellow-400 rounded-full" />
              <span className="text-xs text-yellow-400">{openIntentCount} open intent{openIntentCount !== 1 ? "s" : ""}</span>
            </div>
          )}
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
          <div key={card.label} className="glass rounded-lg p-4">
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
          <div className="glass rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">No insights yet. Run a sentinel check or wait for trajectory patterns.</p>
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

      {/* Unmet Intents Section */}
      <div className="mb-6">
        <div className="flex items-center gap-2 mb-3">
          <h2 className="text-base font-semibold text-zinc-200 tracking-tight">Unmet Intents</h2>
          {openIntentCount > 0 && (
            <span className="text-[10px] font-mono bg-yellow-500/20 text-yellow-400 px-1.5 py-0.5 rounded-full">
              {openIntentCount}
            </span>
          )}
        </div>
        {!unmetIntents || unmetIntents.intents.length === 0 ? (
          <div className="glass rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">No unmet intents detected. The system is meeting all agent needs.</p>
          </div>
        ) : (
          <div className="grid grid-cols-1 md:grid-cols-2 gap-2">
            {unmetIntents.intents.map((intent, i) => (
              <div
                key={`${intent.entity_type}-${intent.error_pattern}-${i}`}
                className={`bg-[#111115] rounded-lg p-3.5 ${
                  intent.status === "open" ? "border-l-2 border-yellow-500/50" : "border-l-2 border-teal-500/30"
                }`}
              >
                <div className="flex items-center gap-2 mb-2">
                  <span className="text-sm font-medium text-zinc-200">{intent.entity_type}</span>
                  <span className={`text-[10px] font-medium px-1.5 py-0.5 rounded ${
                    errorPatternColors[intent.error_pattern] ?? errorPatternColors.Other
                  }`}>
                    {intent.error_pattern}
                  </span>
                  <span className={`text-[10px] px-1.5 py-0.5 rounded ml-auto ${
                    intent.status === "open"
                      ? "bg-yellow-500/15 text-yellow-400"
                      : "bg-teal-500/15 text-teal-400"
                  }`}>
                    {intent.status}
                  </span>
                </div>
                <p className="text-xs text-zinc-400 mb-2">{intent.recommendation}</p>
                <div className="flex items-center gap-4 text-[10px] text-zinc-600">
                  <span>Failures: <span className="text-zinc-400 font-mono">{intent.failure_count}</span></span>
                  <span>First: <span className="text-zinc-400 font-mono">{intent.first_seen?.slice(0, 10) ?? "—"}</span></span>
                  <span>Last: <span className="text-zinc-400 font-mono">{intent.last_seen?.slice(0, 10) ?? "—"}</span></span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Records with tab filter */}
      <div className="mb-6">
        <div className="flex items-center gap-4 mb-3">
          <h2 className="text-base font-semibold text-zinc-200 tracking-tight">Records</h2>
          <div className="flex gap-1">
            {(["all", "observations", "problems", "analyses", "decisions", "insights"] as const).map((tab) => (
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
          <div className="glass rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">No records found.</p>
          </div>
        ) : (
          <div className="glass rounded-lg overflow-hidden max-h-96 overflow-y-auto">
            {filteredRecords.map((record) => (
              <ExpandableRecord key={record.id} record={record} />
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
          <div className="glass rounded-lg p-6 text-center">
            <p className="text-sm text-zinc-500">Click &quot;Run Health Check&quot; to check system health and generate insights.</p>
          </div>
        ) : sentinelResult.alerts_count === 0 && sentinelResult.insights_count === 0 ? (
          <div className="bg-teal-500/5 rounded-lg p-4 flex items-center gap-3">
            <svg className="w-5 h-5 text-teal-400" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
            </svg>
            <span className="text-sm text-teal-400">All checks passed. No alerts or new insights.</span>
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
            {sentinelResult.insights_count > 0 && (
              <div className="bg-yellow-500/5 rounded-lg p-3.5">
                <span className="text-xs text-yellow-400">
                  Generated {sentinelResult.insights_count} new insight{sentinelResult.insights_count !== 1 ? "s" : ""} from trajectory analysis
                </span>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
