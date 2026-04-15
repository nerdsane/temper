"use client";

import {
  createContext,
  useContext,
  useEffect,
  useRef,
  useState,
  useCallback,
} from "react";
import Link from "next/link";
import { fetchAllDecisions, subscribeAllPendingDecisions } from "./api";
import type { PendingDecision } from "./types";

interface Toast {
  id: string;
  decision: PendingDecision;
  createdAt: number;
}

interface DecisionNotifierState {
  pendingCount: number;
  toasts: Toast[];
  dismiss: (id: string) => void;
  dismissAll: () => void;
}

const DecisionNotifierContext = createContext<DecisionNotifierState>({
  pendingCount: 0,
  toasts: [],
  dismiss: () => {},
  dismissAll: () => {},
});

const TOAST_TTL = 15_000;
const MAX_INDIVIDUAL_TOASTS = 3;

function ToastCard({
  toast,
  onDismiss,
}: {
  toast: Toast;
  onDismiss: (id: string) => void;
}) {
  const d = toast.decision;
  return (
    <div className="glass rounded-[2px] p-3 animate-slide-in-up max-w-sm">
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-center gap-2 min-w-0">
          <div className="w-2 h-2 rounded-full bg-[var(--color-accent-pink)] animate-pulse flex-shrink-0" />
          <span className="text-xs font-mono text-[var(--color-text-primary)] truncate">
            {d.agent_id}
          </span>
        </div>
        <button
          onClick={() => onDismiss(toast.id)}
          className="text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] text-xs flex-shrink-0"
        >
          &times;
        </button>
      </div>
      <div className="mt-1.5 text-[11px] text-[var(--color-text-secondary)]">
        <span className="text-[var(--color-accent-teal)] font-mono">{d.action}</span>
        {" on "}
        <span className="font-mono">
          {d.resource_type}::{d.resource_id}
        </span>
      </div>
      <div className="mt-2">
        <Link
          href="/decisions"
          className="text-[11px] text-[var(--color-accent-teal)] hover:text-[var(--color-accent-teal)] transition-colors"
        >
          Review &rarr;
        </Link>
      </div>
    </div>
  );
}

function AggregateToast({
  count,
  onDismiss,
}: {
  count: number;
  onDismiss: () => void;
}) {
  return (
    <div className="glass rounded-[2px] p-3 animate-slide-in-up max-w-sm">
      <div className="flex items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <div className="w-2 h-2 rounded-full bg-[var(--color-accent-pink)] animate-pulse" />
          <span className="text-xs text-[var(--color-text-primary)]">
            {count} decisions need approval
          </span>
        </div>
        <button
          onClick={onDismiss}
          className="text-[var(--color-text-muted)] hover:text-[var(--color-text-secondary)] text-xs"
        >
          &times;
        </button>
      </div>
      <div className="mt-2">
        <Link
          href="/decisions"
          className="text-[11px] text-[var(--color-accent-teal)] hover:text-[var(--color-accent-teal)] transition-colors"
        >
          Review all &rarr;
        </Link>
      </div>
    </div>
  );
}

function ToastContainer({
  toasts,
  onDismiss,
  onDismissAll,
}: {
  toasts: Toast[];
  onDismiss: (id: string) => void;
  onDismissAll: () => void;
}) {
  if (toasts.length === 0) return null;

  return (
    <div className="fixed bottom-4 right-4 z-50 flex flex-col gap-2">
      {toasts.length <= MAX_INDIVIDUAL_TOASTS ? (
        toasts.map((t) => (
          <ToastCard key={t.id} toast={t} onDismiss={onDismiss} />
        ))
      ) : (
        <AggregateToast count={toasts.length} onDismiss={onDismissAll} />
      )}
    </div>
  );
}

export function DecisionNotifierProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [pendingCount, setPendingCount] = useState(0);
  const seenIds = useRef(new Set<string>());

  const dismiss = useCallback((id: string) => {
    setToasts((prev) => prev.filter((t) => t.id !== id));
  }, []);

  const dismissAll = useCallback(() => {
    setToasts([]);
  }, []);

  // Auto-dismiss expired toasts
  useEffect(() => {
    if (toasts.length === 0) return;
    const interval = setInterval(() => {
      const now = Date.now();
      setToasts((prev) => prev.filter((t) => now - t.createdAt < TOAST_TTL));
    }, 1000);
    return () => clearInterval(interval);
  }, [toasts.length]);

  // Initial fetch for pending count
  useEffect(() => {
    const fetchInitial = async () => {
      try {
        const data = await fetchAllDecisions({ status: "pending" });
        setPendingCount(data.pending_count ?? data.decisions.length);
      } catch { /* ignore fetch errors */ }
    };
    fetchInitial();
  }, []);

  // SSE subscription (best-effort — may fail without admin headers)
  useEffect(() => {
    const cleanup = subscribeAllPendingDecisions((decision) => {
      if (seenIds.current.has(decision.id)) return;
      seenIds.current.add(decision.id);
      setPendingCount((c) => c + 1);
      setToasts((prev) => [
        ...prev,
        { id: decision.id, decision, createdAt: Date.now() },
      ]);
    });
    return cleanup;
  }, []);

  return (
    <DecisionNotifierContext.Provider
      value={{ pendingCount, toasts, dismiss, dismissAll }}
    >
      {children}
      <ToastContainer
        toasts={toasts}
        onDismiss={dismiss}
        onDismissAll={dismissAll}
      />
    </DecisionNotifierContext.Provider>
  );
}

export function useDecisionNotifier() {
  return useContext(DecisionNotifierContext);
}
