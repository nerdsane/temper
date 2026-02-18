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
            levels: match.levels.map((l) => ({
              level: l.level,
              passed: l.passed,
              summary: l.summary,
            })),
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
    <div>
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <div className="flex items-center gap-2 text-sm text-gray-500 mb-1">
            <Link href="/" className="hover:text-gray-300">Dashboard</Link>
            <span>/</span>
            <span className="text-gray-300">Verification</span>
            <span>/</span>
            <span className="text-gray-300">{entity}</span>
          </div>
          <h1 className="text-2xl font-bold text-gray-100">
            Verification: {entity}
          </h1>
          <p className="text-sm text-gray-500 mt-1">
            5-level verification cascade (L0 SMT, L1 Model Check, L2 DST, L2b Actor Sim, L3 PropTest)
          </p>
        </div>
        <div className="flex gap-3">
          <Link
            href={`/specs/${entity}`}
            className="px-4 py-2 bg-gray-800 hover:bg-gray-700 text-gray-300 text-sm rounded-md transition-colors border border-gray-700"
          >
            View Spec
          </Link>
          <button
            onClick={handleRunVerification}
            disabled={loading}
            className="px-4 py-2 bg-blue-600 hover:bg-blue-700 disabled:bg-blue-800 disabled:cursor-not-allowed text-white text-sm rounded-md transition-colors"
          >
            {loading ? "Running..." : result ? "Re-run Verification" : "Run Verification"}
          </button>
        </div>
      </div>

      {/* Loading state */}
      {loading && (
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div className="text-gray-500 text-sm mb-2">Running verification cascade...</div>
            <div className="flex gap-1 justify-center">
              {[0, 1, 2, 3, 4].map((i) => (
                <div
                  key={i}
                  className="w-2 h-2 bg-blue-500 rounded-full animate-pulse"
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
            <div className="mb-4 text-xs text-gray-500">
              {result ? "Showing cached results from background verification. Click \"Re-run Verification\" to run again." : ""}
            </div>
          )}
          <CascadeResults levels={result.levels} allPassed={result.all_passed} />
        </>
      )}

      {/* Initial prompt (only when no cached results and not loading) */}
      {!loading && !result && !error && !cachedLoading && (
        <div className="text-center py-12">
          <div className="inline-flex items-center justify-center w-12 h-12 rounded-full bg-gray-800 border border-gray-700 mb-4">
            <svg className="w-6 h-6 text-gray-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
            </svg>
          </div>
          <p className="text-gray-400 mb-4">Click &quot;Run Verification&quot; to start the cascade</p>
          <button
            onClick={handleRunVerification}
            className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm rounded-md transition-colors"
          >
            Run Verification
          </button>
        </div>
      )}

      {/* Loading cached results */}
      {cachedLoading && !loading && !result && (
        <div className="flex items-center justify-center h-32">
          <div className="text-gray-500 text-sm">Checking for cached verification results...</div>
        </div>
      )}
    </div>
  );
}
