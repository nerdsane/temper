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
  Observation: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  Problem: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  Analysis: "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  Decision: "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  Insight: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
};

const errorPatternColors: Record<string, string> = {
  EntitySetNotFound: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  AuthzDenied: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  ActionNotFound: "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  GuardRejected: "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]",
  Other: "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]",
};

type RecordTab = "all" | "observations" | "problems" | "analyses" | "decisions" | "insights";

function priorityBadge(score: number): { bg: string; label: string } {
  if (score > 0.7) return { bg: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]", label: "high" };
  if (score >= 0.3) return { bg: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]", label: "medium" };
  return { bg: "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]", label: "low" };
}

function TrendArrow({ trend }: { trend: string }) {
  if (trend === "growing") {
    return <span className="text-[var(--color-accent-lime)] text-xs" aria-label="growing trend">&#9650;</span>;
  }
  if (trend === "declining") {
    return <span className="text-[var(--color-accent-pink)] text-xs" aria-label="declining trend">&#9660;</span>;
  }
  return <span className="text-[var(--color-text-muted)] text-xs" aria-label="stable trend">&#8212;</span>;
}

function RecordDetail({ detail }: { detail: EvolutionRecordDetail }) {
  return (
    <div className="px-4 py-3 bg-[color-mix(in_srgb,var(--color-bg-surface)_50%,transparent)] border-t border-[var(--color-border)]">
      {detail.derived_from && (
        <div className="text-xs text-[var(--color-text-secondary)] mb-2">
          Derived from: <span className="font-mono text-[var(--color-text-secondary)]">{detail.derived_from}</span>
        </div>
      )}
      {/* O-Record fields */}
      {detail.evidence_query && (
        <div className="mb-2">
          <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider mb-1">Evidence</div>
          <pre className="text-xs text-[var(--color-text-secondary)] font-mono bg-black/30 rounded p-2 overflow-x-auto">{detail.evidence_query}</pre>
        </div>
      )}
      {detail.threshold_field && (
        <div className="flex gap-4 text-xs text-[var(--color-text-secondary)] mb-2">
          <span>Field: <span className="text-[var(--color-text-secondary)] font-mono">{detail.threshold_field}</span></span>
          {detail.threshold_value != null && <span>Threshold: <span className="text-[var(--color-text-secondary)] font-mono">{detail.threshold_value}</span></span>}
          {detail.observed_value != null && <span>Observed: <span className="text-[var(--color-accent-pink)] font-mono">{detail.observed_value}</span></span>}
        </div>
      )}
      {/* A-Record fields */}
      {detail.root_cause && (
        <div className="mb-2">
          <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider mb-1">Root Cause</div>
          <p className="text-xs text-[var(--color-text-secondary)]">{detail.root_cause}</p>
        </div>
      )}
      {detail.options && detail.options.length > 0 && (
        <div className="mb-2">
          <div className="text-[10px] text-[var(--color-text-muted)] uppercase tracking-wider mb-1">Options</div>
          {detail.options.map((opt, i) => (
            <div key={i} className="bg-black/20 rounded p-2 mb-1 text-xs">
              <p className="text-[var(--color-text-secondary)]">{opt.description}</p>
              {opt.spec_diff && (
                <pre className="text-[10px] text-[var(--color-text-secondary)] font-mono mt-1 max-h-32 overflow-y-auto">{opt.spec_diff}</pre>
              )}
              <div className="flex gap-3 mt-1 text-[var(--color-text-muted)]">
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
              detail.decision === "Approved" ? "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]" :
              detail.decision === "Rejected" ? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]" :
              "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"
            }`}>{detail.decision}</span>
            {detail.decided_by && <span className="text-xs text-[var(--color-text-secondary)]">by {detail.decided_by}</span>}
          </div>
          {detail.rationale && <p className="text-xs text-[var(--color-text-secondary)] mt-1">{detail.rationale}</p>}
        </div>
      )}
      {/* I-Record fields */}
      {detail.signal && (
        <div className="flex gap-4 text-xs text-[var(--color-text-secondary)]">
          <span>Intent: <span className="text-[var(--color-text-secondary)] font-mono">{detail.signal.intent}</span></span>
          <span>Volume: <span className="text-[var(--color-text-secondary)] font-mono">{detail.signal.volume}</span></span>
          <span>Rate: <span className="text-[var(--color-text-secondary)] font-mono">{Math.round(detail.signal.success_rate * 100)}%</span></span>
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
        className="w-full flex items-center gap-3 px-3.5 py-2.5 border-b border-[var(--color-border)] last:border-b-0 hover:bg-[var(--color-bg-elevated)] transition-colors text-left"
      >
        <span className="text-[10px] text-[var(--color-text-muted)] flex-shrink-0">{expanded ? "▼" : "▶"}</span>
        <span className={`text-xs font-medium px-1.5 py-0.5 rounded flex-shrink-0 ${recordTypeColors[record.record_type] ?? "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"}`}>
          {record.record_type}
        </span>
        <span className="text-xs text-[var(--color-text-secondary)] font-mono flex-shrink-0">{record.id?.slice(0, 12) ?? "—"}</span>
        {record.source && (
          <span className="text-xs text-[var(--color-text-secondary)] truncate">{record.source}</span>
        )}
        {record.recommendation && (
          <span className="text-xs text-[var(--color-text-secondary)] truncate">{record.recommendation}</span>
        )}
        <span className={`text-xs px-1.5 py-0.5 rounded ml-auto flex-shrink-0 ${
          record.status === "active" || record.status === "Open" ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]" : "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"
        }`}>
          {record.status}
        </span>
      </button>
      {expanded && (
        loading ? (
          <div className="px-4 py-3 bg-[color-mix(in_srgb,var(--color-bg-surface)_50%,transparent)] border-t border-[var(--color-border)]">
            <span className="text-xs text-[var(--color-text-muted)] animate-pulse">Loading...</span>
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
    interval: 30000,
    enabled: !initialLoading && !initialError,
  });

  const insightsPoll = usePolling<EvolutionInsightsResponse>({
    fetcher: fetchEvolutionInsights,
    interval: 30000,
    enabled: !initialLoading && !initialError,
  });

  const unmetPoll = usePolling<UnmetIntentsResponse>({
    fetcher: fetchUnmetIntents,
    interval: 30000,
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
        <div className="h-6 bg-[var(--color-border)] rounded w-36 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-64 mb-6" />
        <div className="grid grid-cols-5 gap-3 mb-6">
          {[0, 1, 2, 3, 4].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
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
    { label: "Observations", value: records?.total_observations ?? 0, color: "text-[var(--color-accent-teal)]" },
    { label: "Problems", value: records?.total_problems ?? 0, color: "text-[var(--color-accent-pink)]" },
    { label: "Analyses", value: records?.total_analyses ?? 0, color: "text-[var(--color-accent-lime)]" },
    { label: "Decisions", value: records?.total_decisions ?? 0, color: "text-[var(--color-accent-lime)]" },
    { label: "Insights", value: records?.total_insights ?? 0, color: "text-[var(--color-accent-pink)]" },
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
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">Evolution</h1>
          <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
            O-P-A-D-I record chain, insights, unmet intents, and sentinel health
          </p>
        </div>
        <div className="flex items-center gap-3">
          {openIntentCount > 0 && (
            <div className="flex items-center gap-1.5">
              <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full" />
              <span className="text-xs text-[var(--color-accent-pink)]">{openIntentCount} open intent{openIntentCount !== 1 ? "s" : ""}</span>
            </div>
          )}
          {sentinelResult && (
            <div className="flex items-center gap-1.5">
              {sentinelResult.alerts_count === 0 ? (
                <>
                  <svg className="w-3.5 h-3.5 text-[var(--color-accent-teal)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                  </svg>
                  <span className="text-xs text-[var(--color-accent-teal)]">Healthy</span>
                </>
              ) : (
                <>
                  <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full animate-pulse" />
                  <span className="text-xs text-[var(--color-accent-pink)]">{sentinelResult.alerts_count} alert{sentinelResult.alerts_count !== 1 ? "s" : ""}</span>
                </>
              )}
            </div>
          )}
        </div>
      </div>

      {/* Record Summary */}
      <div className="grid grid-cols-5 gap-3 mb-6">
        {summaryCards.map((card) => (
          <div key={card.label} className="glass rounded-[2px] p-4">
            <div className="text-xs text-[var(--color-text-muted)]">{card.label}</div>
            <div className={`text-4xl font-bold font-mono mt-0.5 ${card.color}`}>
              {card.value}
            </div>
          </div>
        ))}
      </div>

      {/* Insights Panel */}
      <div className="mb-6">
        <h2 className="text-base font-semibold text-[var(--color-text-primary)] mb-3 tracking-tight">Insights</h2>
        {!insights || insights.insights.length === 0 ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">No insights yet. Run a sentinel check or wait for trajectory patterns.</p>
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
                    className="bg-[var(--color-bg-surface)] rounded-[2px] p-3.5"
                  >
                    <div className="flex items-center gap-2 mb-2">
                      <span className={`text-xs font-medium px-1.5 py-0.5 rounded ${recordTypeColors.Insight ?? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"}`}>
                        {insight.category}
                      </span>
                      <span className={`text-xs font-medium px-1.5 py-0.5 rounded ${badge.bg}`}>
                        {badge.label}
                      </span>
                      <span className="text-xs font-mono text-[var(--color-text-muted)] ml-auto">
                        {insight.priority_score.toFixed(2)}
                      </span>
                    </div>
                    <p className="text-sm text-[var(--color-text-secondary)] mb-2">{insight.recommendation}</p>
                    <div className="flex items-center gap-4 text-xs text-[var(--color-text-muted)]">
                      <span>Intent: <span className="text-[var(--color-text-secondary)] font-mono">{insight.signal.intent}</span></span>
                      <span>Vol: <span className="text-[var(--color-text-secondary)] font-mono">{insight.signal.volume}</span></span>
                      <span>Rate: <span className="text-[var(--color-text-secondary)] font-mono">{Math.round(insight.signal.success_rate * 100)}%</span></span>
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
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">Unmet Intents</h2>
          {openIntentCount > 0 && (
            <span className="text-[10px] font-mono bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded-full">
              {openIntentCount}
            </span>
          )}
        </div>
        {!unmetIntents || unmetIntents.intents.length === 0 ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">No unmet intents detected. The system is meeting all agent needs.</p>
          </div>
        ) : (
          <div className="grid grid-cols-1 md:grid-cols-2 gap-2">
            {unmetIntents.intents.map((intent, i) => (
              <div
                key={`${intent.entity_type}-${intent.error_pattern}-${i}`}
                className={`bg-[var(--color-bg-surface)] rounded-[2px] p-3.5 ${
                  intent.status === "open" ? "border-l-2 border-[var(--color-accent-pink)]/50" : "border-l-2 border-[var(--color-accent-teal)]/30"
                }`}
              >
                <div className="flex items-center gap-2 mb-2">
                  <span className="text-sm font-medium text-[var(--color-text-primary)]">{intent.entity_type}</span>
                  <span className={`text-[10px] font-medium px-1.5 py-0.5 rounded ${
                    errorPatternColors[intent.error_pattern] ?? errorPatternColors.Other
                  }`}>
                    {intent.error_pattern}
                  </span>
                  <span className={`text-[10px] px-1.5 py-0.5 rounded ml-auto ${
                    intent.status === "open"
                      ? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]"
                      : "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]"
                  }`}>
                    {intent.status}
                  </span>
                </div>
                <p className="text-xs text-[var(--color-text-secondary)] mb-2">{intent.recommendation}</p>
                <div className="flex items-center gap-4 text-[10px] text-[var(--color-text-muted)]">
                  <span>Failures: <span className="text-[var(--color-text-secondary)] font-mono">{intent.failure_count}</span></span>
                  <span>First: <span className="text-[var(--color-text-secondary)] font-mono">{intent.first_seen?.slice(0, 10) ?? "—"}</span></span>
                  <span>Last: <span className="text-[var(--color-text-secondary)] font-mono">{intent.last_seen?.slice(0, 10) ?? "—"}</span></span>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Records with tab filter */}
      <div className="mb-6">
        <div className="flex items-center gap-4 mb-3">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">Records</h2>
          <div className="flex gap-1">
            {(["all", "observations", "problems", "analyses", "decisions", "insights"] as const).map((tab) => (
              <button
                key={tab}
                onClick={() => setActiveTab(tab)}
                className={`text-xs px-2 py-1 rounded transition-colors ${
                  activeTab === tab
                    ? "text-[var(--color-accent-teal)] border-b-2 border-[var(--color-accent-teal)]"
                    : "text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)]"
                }`}
              >
                {tab.charAt(0).toUpperCase() + tab.slice(1)}
              </button>
            ))}
          </div>
        </div>
        {filteredRecords.length === 0 ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">No records found.</p>
          </div>
        ) : (
          <div className="glass rounded-[2px] overflow-hidden max-h-96 overflow-y-auto">
            {filteredRecords.map((record) => (
              <ExpandableRecord key={record.id} record={record} />
            ))}
          </div>
        )}
      </div>

      {/* Sentinel Panel */}
      <div>
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-base font-semibold text-[var(--color-text-primary)] tracking-tight">Sentinel Health Check</h2>
          <button
            onClick={handleSentinelCheck}
            disabled={sentinelLoading}
            className="px-3 py-1.5 bg-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal)] disabled:bg-[var(--color-accent-teal)]/50 text-[var(--color-bg-primary)] text-xs rounded-[2px] transition-colors"
          >
            {sentinelLoading ? "Checking..." : "Run Health Check"}
          </button>
        </div>
        {sentinelResult === null ? (
          <div className="glass rounded-[2px] p-6 text-center">
            <p className="text-sm text-[var(--color-text-secondary)]">Click &quot;Run Health Check&quot; to check system health and generate insights.</p>
          </div>
        ) : sentinelResult.alerts_count === 0 && sentinelResult.insights_count === 0 ? (
          <div className="bg-[var(--color-accent-teal-dim)] rounded-[2px] p-4 flex items-center gap-3">
            <svg className="w-5 h-5 text-[var(--color-accent-teal)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
            </svg>
            <span className="text-sm text-[var(--color-accent-teal)]">All checks passed. No alerts or new insights.</span>
          </div>
        ) : (
          <div className="space-y-2">
            {sentinelResult.alerts.map((alert, i) => (
              <div
                key={`${alert.rule}-${i}`}
                className="bg-[var(--color-accent-pink-dim)] rounded-[2px] p-3.5 animate-pulse-once"
              >
                <div className="flex items-center gap-2 mb-1.5">
                  <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full animate-pulse" />
                  <span className="text-sm font-medium text-[var(--color-accent-pink)]">{alert.rule}</span>
                  <span className={`text-[10px] px-1.5 py-0.5 rounded ml-auto ${recordTypeColors[alert.classification] ?? "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"}`}>
                    {alert.classification}
                  </span>
                </div>
                <div className="flex items-center gap-4 text-xs text-[var(--color-text-muted)]">
                  <span>Source: <span className="text-[var(--color-text-secondary)] font-mono">{alert.source}</span></span>
                  <span>Threshold: <span className="text-[var(--color-text-secondary)] font-mono">{alert.threshold}</span></span>
                  <span>Observed: <span className="text-[var(--color-accent-pink)] font-mono">{alert.observed}</span></span>
                </div>
              </div>
            ))}
            {sentinelResult.insights_count > 0 && (
              <div className="bg-[var(--color-accent-pink-dim)] rounded-[2px] p-3.5">
                <span className="text-xs text-[var(--color-accent-pink)]">
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
