"use client";

import { useState, useEffect, useRef, useCallback } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { runVerification, fetchVerificationStatus, subscribeDesignTimeEvents } from "@/lib/api";
import type { VerificationResult, DesignTimeEvent } from "@/lib/types";
import CascadeResults from "@/components/CascadeResults";
import ErrorDisplay from "@/components/ErrorDisplay";

type StepStatus = "pending" | "running" | "passed" | "failed";

interface StepState {
  label: string;
  levelPrefix: string;
  status: StepStatus;
}

const INITIAL_STEPS: StepState[] = [
  { label: "L0 SMT", levelPrefix: "Level 0", status: "pending" },
  { label: "L1 Model Check", levelPrefix: "Level 1", status: "pending" },
  { label: "L2 DST", levelPrefix: "Level 2:", status: "pending" },
  { label: "L2b Actor Sim", levelPrefix: "Level 2b", status: "pending" },
  { label: "L3 PropTest", levelPrefix: "Level 3", status: "pending" },
];

export default function VerificationPage() {
  const params = useParams();
  const entity = params.entity as string;
  const [result, setResult] = useState<VerificationResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cachedLoading, setCachedLoading] = useState(true);
  const [steps, setSteps] = useState<StepState[]>(INITIAL_STEPS);
  const cleanupRef = useRef<(() => void) | null>(null);

  // On mount, check if background verification already completed for this entity
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const status = await fetchVerificationStatus();
        if (cancelled) return;
        const match = status.entities.find((e) => e.entity_type === entity);
        if (match && match.status !== "pending" && match.status !== "running" && match.levels) {
          setResult({
            all_passed: match.status === "passed",
            levels: match.levels,
          });
        }
      } catch {
        // Ignore — user can still run verification manually
      } finally {
        if (!cancelled) setCachedLoading(false);
      }
    })();
    return () => { cancelled = true; };
  }, [entity]);

  // Cleanup SSE on unmount
  useEffect(() => {
    return () => {
      if (cleanupRef.current) cleanupRef.current();
    };
  }, []);

  const handleDesignTimeEvent = useCallback((event: DesignTimeEvent) => {
    if (event.entity_type !== entity) return;

    if (event.kind === "verify_started") {
      // Mark first step as running
      setSteps((prev) => prev.map((s, i) =>
        i === 0 ? { ...s, status: "running" } : s,
      ));
    }

    if (event.kind === "verify_level" && event.level) {
      setSteps((prev) => {
        const updated = [...prev];
        // Find the matching step by level prefix
        const idx = updated.findIndex((s) => event.level!.startsWith(s.levelPrefix));
        if (idx >= 0) {
          updated[idx] = {
            ...updated[idx],
            status: event.passed ? "passed" : "failed",
          };
          // Mark next step as running if current passed
          if (event.passed && idx + 1 < updated.length && updated[idx + 1].status === "pending") {
            updated[idx + 1] = { ...updated[idx + 1], status: "running" };
          }
        }
        return updated;
      });
    }

    if (event.kind === "verify_done") {
      // Mark any remaining running steps based on passed
      setSteps((prev) => prev.map((s) =>
        s.status === "running" ? { ...s, status: event.passed ? "passed" : "failed" } : s,
      ));
    }
  }, [entity]);

  const handleRunVerification = async () => {
    setLoading(true);
    setError(null);
    setResult(null);
    setSteps(INITIAL_STEPS);

    // Subscribe to SSE for per-level progress
    if (cleanupRef.current) cleanupRef.current();
    cleanupRef.current = subscribeDesignTimeEvents(handleDesignTimeEvent);

    try {
      const data = await runVerification(entity);
      setResult(data);
      // Update steps from final result
      setSteps((prev) => prev.map((step) => {
        const matchingLevel = data.levels.find((l) =>
          l.level.startsWith(step.levelPrefix),
        );
        if (matchingLevel) {
          return { ...step, status: matchingLevel.passed ? "passed" : "failed" };
        }
        return step;
      }));
    } catch (err) {
      setError(err instanceof Error ? err.message : `Verification failed for "${entity}"`);
      setSteps((prev) => prev.map((s) =>
        s.status === "running" || s.status === "pending" ? { ...s, status: "failed" } : s,
      ));
    } finally {
      setLoading(false);
      if (cleanupRef.current) {
        cleanupRef.current();
        cleanupRef.current = null;
      }
    }
  };

  const hasAnyProgress = steps.some((s) => s.status !== "pending");

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-5">
        <div>
          <div className="flex items-center gap-1.5 text-[12px] text-[var(--color-text-muted)] mb-1">
            <Link href="/" className="hover:text-[var(--color-text-secondary)] transition-colors">Dashboard</Link>
            <span>/</span>
            <span className="text-[var(--color-text-secondary)]">Verification</span>
            <span>/</span>
            <span className="text-[var(--color-text-secondary)]">{entity}</span>
          </div>
          <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">
            Verification: {entity}
          </h1>
          <p className="text-[13px] text-[var(--color-text-muted)] mt-0.5">
            5-level verification cascade (L0 SMT, L1 Model Check, L2 DST, L2b Actor Sim, L3 PropTest)
          </p>
        </div>
        <div className="flex gap-2.5">
          <Link
            href={`/specs/${entity}`}
            className="px-3.5 py-1.5 bg-[var(--color-bg-elevated)] hover:bg-[var(--color-border-hover)] text-[var(--color-text-secondary)] text-[13px] rounded-[2px] transition-colors"
          >
            View Spec
          </Link>
          <button
            onClick={handleRunVerification}
            disabled={loading}
            className="px-3.5 py-1.5 bg-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal)] disabled:bg-[var(--color-accent-teal)]/50 disabled:cursor-not-allowed text-[var(--color-bg-primary)] text-[13px] rounded-[2px] transition-colors"
          >
            {loading ? "Running..." : result ? "Re-run Verification" : "Run Verification"}
          </button>
        </div>
      </div>

      {/* Stepper Progress */}
      {(loading || hasAnyProgress) && (
        <div className="glass rounded-[2px] p-5 mb-5">
          <div className="flex items-center gap-1">
            {steps.map((step, i) => (
              <div key={step.label} className="flex items-center flex-1 last:flex-initial">
                {/* Step indicator */}
                <div className="flex flex-col items-center">
                  <div
                    className={`w-8 h-8 rounded-full flex items-center justify-center text-[11px] font-mono transition-all duration-300 ${
                      step.status === "passed"
                        ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] ring-1 ring-[var(--color-accent-teal)]/40"
                        : step.status === "failed"
                          ? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] ring-1 ring-[var(--color-accent-pink)]/40"
                          : step.status === "running"
                            ? "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] ring-1 ring-[var(--color-accent-pink)]/40 animate-pulse"
                            : "bg-[var(--color-bg-elevated)] text-[var(--color-text-muted)] ring-1 ring-[var(--color-border)]"
                    }`}
                  >
                    {step.status === "passed" ? (
                      <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2.5} d="M5 13l4 4L19 7" />
                      </svg>
                    ) : step.status === "failed" ? (
                      <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                        <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2.5} d="M6 18L18 6M6 6l12 12" />
                      </svg>
                    ) : step.status === "running" ? (
                      <div className="w-2 h-2 bg-[var(--color-accent-pink)] rounded-full" />
                    ) : (
                      <span>{i}</span>
                    )}
                  </div>
                  <span
                    className={`text-[10px] mt-1.5 whitespace-nowrap ${
                      step.status === "passed"
                        ? "text-[var(--color-accent-teal)]"
                        : step.status === "failed"
                          ? "text-[var(--color-accent-pink)]"
                          : step.status === "running"
                            ? "text-[var(--color-accent-pink)]"
                            : "text-[var(--color-text-muted)]"
                    }`}
                  >
                    {step.label}
                  </span>
                </div>
                {/* Connector line */}
                {i < steps.length - 1 && (
                  <div
                    className={`flex-1 h-px mx-2 mt-[-18px] transition-colors duration-300 ${
                      step.status === "passed"
                        ? "bg-[var(--color-accent-teal)]/40"
                        : step.status === "failed"
                          ? "bg-[var(--color-accent-pink)]/40"
                          : "bg-[var(--color-bg-elevated)]"
                    }`}
                  />
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Error state */}
      {!loading && error && (
        <ErrorDisplay
          title="Verification error"
          message={error}
          retry={handleRunVerification}
        />
      )}

      {/* Results (from background verification or manual run) */}
      {!loading && result && (
        <>
          {!cachedLoading && !hasAnyProgress && (
            <div className="mb-3 text-[11px] text-[var(--color-text-muted)]">
              Showing cached results from background verification. Click &quot;Re-run Verification&quot; to run again.
            </div>
          )}
          <CascadeResults levels={result.levels} allPassed={result.all_passed} />
        </>
      )}

      {/* Initial prompt (only when no cached results and not loading) */}
      {!loading && !result && !error && !cachedLoading && (
        <div className="text-center py-10">
          <div className="inline-flex items-center justify-center w-10 h-10 rounded-full bg-[var(--color-bg-elevated)] mb-4">
            <svg className="w-5 h-5 text-[var(--color-text-muted)]" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
            </svg>
          </div>
          <p className="text-[var(--color-text-secondary)] text-[13px] mb-3">Click &quot;Run Verification&quot; to start the cascade</p>
          <button
            onClick={handleRunVerification}
            className="px-3.5 py-1.5 bg-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal)] text-[var(--color-bg-primary)] text-[13px] rounded-[2px] transition-colors"
          >
            Run Verification
          </button>
        </div>
      )}

      {/* Loading cached results */}
      {cachedLoading && !loading && !result && (
        <div className="flex items-center justify-center h-28">
          <div className="text-[var(--color-text-muted)] text-[13px]">Checking for cached verification results...</div>
        </div>
      )}
    </div>
  );
}
