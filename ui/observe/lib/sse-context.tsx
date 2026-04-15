"use client";
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
  const sourceRef = useRef<EventSource | null>(null);
  const closedRef = useRef(false);
  const retryDelayRef = useRef(1000);

  useEffect(() => {
    closedRef.current = false;

    function connect() {
      if (closedRef.current) return;
      const source = new EventSource("/observe/refresh/stream");
      sourceRef.current = source;

      source.addEventListener("refresh", (e) => {
        retryDelayRef.current = 1000;
        try {
          const { kind } = JSON.parse((e as MessageEvent).data);
          for (const listener of listenersRef.current) {
            if (listener.kinds.includes(kind)) {
              listener.callback();
            }
          }
        } catch { /* ignore parse errors */ }
      });

      source.onopen = () => setConnected(true);
      source.onerror = () => {
        setConnected(false);
        source.close();
        if (!closedRef.current) {
          setTimeout(connect, retryDelayRef.current);
          retryDelayRef.current = Math.min(retryDelayRef.current * 2, 30000);
        }
      };
    }

    connect();
    return () => {
      closedRef.current = true;
      sourceRef.current?.close();
      setConnected(false);
    };
  }, []);

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
