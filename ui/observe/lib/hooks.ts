"use client";

import { useState, useEffect, useRef, useCallback } from "react";

interface UsePollingOptions<T> {
  fetcher: () => Promise<T>;
  interval: number;
  enabled?: boolean;
}

interface UsePollingResult<T> {
  data: T | null;
  error: string | null;
  loading: boolean;
  lastUpdated: Date | null;
  refresh: () => Promise<void>;
}

export function usePolling<T>({
  fetcher,
  interval,
  enabled = true,
}: UsePollingOptions<T>): UsePollingResult<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const fetcherRef = useRef(fetcher);
  const abortRef = useRef<AbortController | null>(null);
  fetcherRef.current = fetcher;

  const doFetch = useCallback(async (showLoading = false) => {
    // Cancel any in-flight request
    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;

    if (showLoading) setLoading(true);
    try {
      const result = await fetcherRef.current();
      if (!controller.signal.aborted) {
        setData(result);
        setError(null);
        setLastUpdated(new Date());
      }
    } catch (err) {
      if (!controller.signal.aborted) {
        setError(err instanceof Error ? err.message : "Unknown error");
      }
    } finally {
      if (!controller.signal.aborted) {
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => {
    if (!enabled) return;
    doFetch(true);
    intervalRef.current = setInterval(() => doFetch(false), interval);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
      abortRef.current?.abort();
    };
  }, [doFetch, interval, enabled]);

  const refresh = useCallback(() => doFetch(false), [doFetch]);

  return { data, error, loading, lastUpdated, refresh };
}

export function useRelativeTime(date: Date | null): string {
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
  const seconds = Math.floor((now - date.getTime()) / 1000);
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ago`;
}
