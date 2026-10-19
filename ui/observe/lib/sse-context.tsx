"use client";
import { createVisibleEventSource } from "./event-source";
import { createContext, useContext, useEffect, useRef, useState, useCallback, type ReactNode } from "react";

type Listener = { kinds: string[]; callback: () => void };

interface SSERefreshContextValue {
  subscribe: (kinds: string[], callback: () => void) => () => void;
  connected: boolean;
}

const SSERefreshContext = createContext<SSERefreshContextValue | null>(null);

export function SSERefreshProvider({ children }: { children: ReactNode }) {
  const listenersRef = useRef<Set<Listener>>(new Set());
  const [connected, setConnected] = useState(false);
  useEffect(() => createVisibleEventSource(
    "/observe/refresh/stream",
    "refresh",
    (raw) => {
      try {
        const { kind } = JSON.parse(raw);
        for (const listener of listenersRef.current) {
          if (listener.kinds.includes(kind)) listener.callback();
        }
      } catch { /* ignore parse errors */ }
    },
    (value) => {
      setConnected(value);
      // Re-fetch state missed while the tab was disconnected.
      if (value) for (const listener of listenersRef.current) listener.callback();
    },
  ), []);

  const subscribe = useCallback((kinds: string[], callback: () => void) => {
    const listener: Listener = { kinds, callback };
    listenersRef.current.add(listener);
    return () => { listenersRef.current.delete(listener); };
  }, []);

  return (
    <SSERefreshContext.Provider value={{ subscribe, connected }}>
      {children}
    </SSERefreshContext.Provider>
  );
}

export function useSSERefreshSubscribe(kinds: string[], callback: () => void) {
  const ctx = useContext(SSERefreshContext);
  const callbackRef = useRef(callback);
  callbackRef.current = callback;

  useEffect(() => {
    if (!ctx) return;
    return ctx.subscribe(kinds, () => callbackRef.current());
  }, [ctx, kinds.join(",")]); // eslint-disable-line react-hooks/exhaustive-deps
}

export function useSSEConnected(): boolean {
  const ctx = useContext(SSERefreshContext);
  return ctx?.connected ?? false;
}
