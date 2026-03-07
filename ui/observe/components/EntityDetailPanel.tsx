"use client";

import { useEffect, useState, useCallback } from "react";
import Link from "next/link";
import { fetchEntityHistory } from "@/lib/api";
import type { EntityHistory } from "@/lib/types";
import StatusBadge from "@/components/StatusBadge";

interface EntityDetailPanelProps {
  entityType: string;
  entityId: string;
  onClose: () => void;
}

export default function EntityDetailPanel({
  entityType,
  entityId,
  onClose,
}: EntityDetailPanelProps) {
  const [history, setHistory] = useState<EntityHistory | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchEntityHistory(entityType, entityId);
      setHistory(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load entity");
    } finally {
      setLoading(false);
    }
  }, [entityType, entityId]);

  useEffect(() => {
    load();
  }, [load]);

  // Close on Escape
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 bg-black/40 z-40"
        onClick={onClose}
        aria-hidden="true"
      />

      {/* Panel */}
      <div role="dialog" aria-label={`${entityType} ${entityId} details`} className="fixed top-0 right-0 h-full w-96 z-50 bg-[#0a0a0c]/95 backdrop-blur-sm border-l border-white/[0.06] animate-slide-in-right overflow-y-auto">
        {/* Header */}
        <div className="sticky top-0 bg-[#0a0a0c]/95 backdrop-blur-sm z-10 px-5 py-4 border-b border-white/[0.06]">
          <div className="flex items-center justify-between">
            <div className="min-w-0">
              <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-0.5">
                {entityType}
              </div>
              <div className="text-sm font-mono text-zinc-200 truncate">
                {entityId}
              </div>
            </div>
            <button
              onClick={onClose}
              aria-label="Close panel"
              className="w-7 h-7 flex items-center justify-center rounded bg-white/[0.04] hover:bg-white/[0.08] text-zinc-500 hover:text-zinc-300 transition-colors flex-shrink-0"
            >
              <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
              </svg>
            </button>
          </div>
        </div>

        {/* Content */}
        <div className="px-5 py-4">
          {loading && (
            <div className="flex items-center justify-center py-12">
              <div className="text-zinc-600 text-[13px]">Loading...</div>
            </div>
          )}

          {!loading && error && (
            <div className="text-center py-8">
              <p className="text-pink-400 text-[13px] mb-2">{error}</p>
              <button
                onClick={load}
                className="text-[12px] text-teal-400 hover:text-teal-300 transition-colors"
              >
                Retry
              </button>
            </div>
          )}

          {!loading && history && (
            <div className="space-y-5">
              {/* Current State */}
              <div>
                <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                  Current State
                </div>
                <StatusBadge status={history.current_state} />
              </div>

              {/* Fields */}
              {history.fields && Object.keys(history.fields).length > 0 && (
                <div>
                  <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                    Fields
                  </div>
                  <div className="glass rounded overflow-hidden">
                    {Object.entries(history.fields).map(([key, value]) => (
                      <div
                        key={key}
                        className="flex items-center justify-between px-3 py-2 border-b border-white/[0.03] last:border-b-0"
                      >
                        <span className="text-[11px] text-zinc-500 font-mono">{key}</span>
                        <span className="text-[11px] text-zinc-300 font-mono truncate ml-3 max-w-[200px]">
                          {String(value)}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Counters */}
              {history.counters && Object.keys(history.counters).length > 0 && (
                <div>
                  <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                    Counters
                  </div>
                  <div className="glass rounded overflow-hidden">
                    {Object.entries(history.counters).map(([key, value]) => (
                      <div
                        key={key}
                        className="flex items-center justify-between px-3 py-2 border-b border-white/[0.03] last:border-b-0"
                      >
                        <span className="text-[11px] text-zinc-500 font-mono">{key}</span>
                        <span className="text-[11px] text-teal-400 font-mono">{value}</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Booleans */}
              {history.booleans && Object.keys(history.booleans).length > 0 && (
                <div>
                  <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                    Booleans
                  </div>
                  <div className="glass rounded overflow-hidden">
                    {Object.entries(history.booleans).map(([key, value]) => (
                      <div
                        key={key}
                        className="flex items-center justify-between px-3 py-2 border-b border-white/[0.03] last:border-b-0"
                      >
                        <span className="text-[11px] text-zinc-500 font-mono">{key}</span>
                        <span className={`text-[11px] font-mono ${value ? "text-teal-400" : "text-zinc-600"}`}>
                          {value ? "true" : "false"}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Lists */}
              {history.lists && Object.keys(history.lists).length > 0 && (
                <div>
                  <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                    Lists
                  </div>
                  <div className="glass rounded overflow-hidden">
                    {Object.entries(history.lists).map(([key, value]) => (
                      <div
                        key={key}
                        className="px-3 py-2 border-b border-white/[0.03] last:border-b-0"
                      >
                        <div className="text-[11px] text-zinc-500 font-mono mb-1">{key}</div>
                        {value.length === 0 ? (
                          <span className="text-[10px] text-zinc-700">(empty)</span>
                        ) : (
                          <div className="flex flex-wrap gap-1">
                            {value.map((item, j) => (
                              <span
                                key={j}
                                className="text-[10px] bg-white/[0.04] text-zinc-400 px-1.5 py-0.5 rounded font-mono"
                              >
                                {item}
                              </span>
                            ))}
                          </div>
                        )}
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Recent Events */}
              {history.events.length > 0 && (
                <div>
                  <div className="text-[10px] text-zinc-600 uppercase tracking-wider mb-1.5">
                    Recent Events ({history.events.length})
                  </div>
                  <div className="glass rounded overflow-hidden max-h-48 overflow-y-auto">
                    {[...history.events].reverse().slice(0, 20).map((event, i) => (
                      <div
                        key={i}
                        className="flex items-center gap-2 px-3 py-2 border-b border-white/[0.03] last:border-b-0"
                      >
                        <span className="text-[10px] text-teal-400 font-mono flex-shrink-0">
                          {event.action}
                        </span>
                        <span className="text-zinc-700 text-[10px]">&rarr;</span>
                        <span className="text-[10px] text-zinc-400 font-mono">
                          {event.to_state}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              )}

              {/* Open Full Page */}
              <div className="pt-2">
                <Link
                  href={`/entities/${entityType}/${entityId}`}
                  className="flex items-center justify-center gap-1.5 w-full py-2 rounded-md bg-white/[0.04] hover:bg-white/[0.08] text-[12px] text-zinc-400 hover:text-zinc-200 transition-colors"
                >
                  Open full page
                  <svg className="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M10 6H6a2 2 0 00-2 2v10a2 2 0 002 2h10a2 2 0 002-2v-4M14 4h6m0 0v6m0-6L10 14" />
                  </svg>
                </Link>
              </div>
            </div>
          )}
        </div>
      </div>
    </>
  );
}
