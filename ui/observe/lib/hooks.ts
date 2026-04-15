"use client";

import { useState, useEffect, useRef, useCallback } from "react";
import { useSSERefreshSubscribe } from "./sse-context";

export interface UsePollingResult<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  lastUpdated: Date | null;
  refresh: () => Promise<void>;
}

interface UseSSERefreshOptions<T> {
  fetcher: () => Promise<T>;
  sseKinds: string[];
  enabled?: boolean;
}

export function useSSERefresh<T>({
  fetcher,
  sseKinds,
  enabled = true,
}: UseSSERefreshOptions<T>): UsePollingResult<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const fetcherRef = useRef(fetcher);
  const abortRef = useRef<AbortController | null>(null);
  fetcherRef.current = fetcher;

  const doFetch = useCallback(async (isInitial: boolean) => {
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    try {
      if (isInitial) setLoading(true);
      const result = await fetcherRef.current();
      if (!controller.signal.aborted) {
        setData(result);
        setError(null);
        setLastUpdated(new Date());
        if (isInitial) setLoading(false);
      }
    } catch (err) {
      if (!controller.signal.aborted) {
        setError(err instanceof Error ? err.message : String(err));
        if (isInitial) setLoading(false);
      }
    }
  }, []);

  // Initial fetch
  useEffect(() => {
    if (!enabled) return;
    doFetch(true);
    return () => { abortRef.current?.abort(); };
  }, [enabled, doFetch]);

  // SSE-driven refetch
  useSSERefreshSubscribe(
    enabled ? sseKinds : [],
    () => { if (enabled) doFetch(false); }
  );

  const refresh = useCallback(async () => { await doFetch(false); }, [doFetch]);

  return { data, error, loading, lastUpdated, refresh };
}

export function useRelativeTime(date: Date | string | null): string {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    // Pause timer when tab is not visible
    let id: ReturnType<typeof setInterval> | null = null;

    function start() {
      if (!id) id = setInterval(() => setNow(Date.now()), 1000);
    }
    function stop() {
      if (id) { clearInterval(id); id = null; }
    }
    function onVisibility() {
      document.hidden ? stop() : start();
    }

    start();
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  if (!date) return "";
  const d = typeof date === "string" ? new Date(date) : date;
  if (isNaN(d.getTime())) return "";
  const seconds = Math.floor((now - d.getTime()) / 1000);
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ago`;
}
