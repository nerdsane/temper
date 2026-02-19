"use client";

import { useState, useEffect } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { runVerification, fetchVerificationStatus } from "@/lib/api";
import type { VerificationResult } from "@/lib/types";
import CascadeResults from "@/components/CascadeResults";
import ErrorDisplay from "@/components/ErrorDisplay";

export default function VerificationPage() {
  const params = useParams();
  const entity = params.entity as string;
  const [result, setResult] = useState<VerificationResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cachedLoading, setCachedLoading] = useState(true);

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

  const handleRunVerification = async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await runVerification(entity);
      setResult(data);
    } catch (err) {
      setError(err instanceof Error ? err.message : `Verification failed for "${entity}"`);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="flex items-center justify-between mb-5">
        <div>
          <div className="flex items-center gap-1.5 text-[12px] text-zinc-600 mb-1">
            <Link href="/" className="hover:text-zinc-400 transition-colors">Dashboard</Link>
            <span>/</span>
            <span className="text-zinc-400">Verification</span>
            <span>/</span>
            <span className="text-zinc-400">{entity}</span>
          </div>
          <h1 className="text-2xl font-bold text-zinc-100 tracking-tight font-display">
            Verification: {entity}
          </h1>
          <p className="text-[13px] text-zinc-600 mt-0.5">
            5-level verification cascade (L0 SMT, L1 Model Check, L2 DST, L2b Actor Sim, L3 PropTest)
          </p>
        </div>
        <div className="flex gap-2.5">
          <Link
            href={`/specs/${entity}`}
            className="px-3.5 py-1.5 bg-white/[0.04] hover:bg-white/[0.08] text-zinc-400 text-[13px] rounded-md transition-colors"
          >
            View Spec
          </Link>
          <button
            onClick={handleRunVerification}
            disabled={loading}
            className="px-3.5 py-1.5 bg-teal-500 hover:bg-teal-400 disabled:bg-teal-600 disabled:cursor-not-allowed text-white text-[13px] rounded-md transition-colors"
          >
            {loading ? "Running..." : result ? "Re-run Verification" : "Run Verification"}
          </button>
        </div>
      </div>

      {/* Loading state */}
      {loading && (
        <div className="flex items-center justify-center h-56">
          <div className="text-center">
            <div className="text-zinc-600 text-[13px] mb-2">Running verification cascade...</div>
            <div className="flex gap-1 justify-center">
              {[0, 1, 2, 3, 4].map((i) => (
                <div
                  key={i}
                  className="w-1.5 h-1.5 bg-teal-500 rounded-full animate-pulse"
                  style={{ animationDelay: `${i * 150}ms` }}
                />
              ))}
            </div>
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
          {!cachedLoading && (
            <div className="mb-3 text-[11px] text-zinc-600">
              {result ? "Showing cached results from background verification. Click \"Re-run Verification\" to run again." : ""}
            </div>
          )}
          <CascadeResults levels={result.levels} allPassed={result.all_passed} />
        </>
      )}

      {/* Initial prompt (only when no cached results and not loading) */}
      {!loading && !result && !error && !cachedLoading && (
        <div className="text-center py-10">
          <div className="inline-flex items-center justify-center w-10 h-10 rounded-full bg-white/[0.04] mb-4">
            <svg className="w-5 h-5 text-zinc-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
            </svg>
          </div>
          <p className="text-zinc-500 text-[13px] mb-3">Click &quot;Run Verification&quot; to start the cascade</p>
          <button
            onClick={handleRunVerification}
            className="px-3.5 py-1.5 bg-teal-500 hover:bg-teal-400 text-white text-[13px] rounded-md transition-colors"
          >
            Run Verification
          </button>
        </div>
      )}

      {/* Loading cached results */}
      {cachedLoading && !loading && !result && (
        <div className="flex items-center justify-center h-28">
          <div className="text-zinc-600 text-[13px]">Checking for cached verification results...</div>
        </div>
      )}
    </div>
  );
}
