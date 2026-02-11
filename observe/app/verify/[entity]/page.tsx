"use client";

import { useEffect, useState } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { runVerification } from "@/lib/api";
import type { VerificationResult } from "@/lib/mock-data";
import CascadeResults from "@/components/CascadeResults";

export default function VerificationPage() {
  const params = useParams();
  const entity = params.entity as string;
  const [result, setResult] = useState<VerificationResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [hasRun, setHasRun] = useState(false);

  const handleRunVerification = async () => {
    setLoading(true);
    const data = await runVerification(entity);
    setResult(data);
    setLoading(false);
    setHasRun(true);
  };

  // Auto-run on mount to show mock data
  useEffect(() => {
    handleRunVerification();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [entity]);

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
            {loading ? "Running..." : "Run Verification"}
          </button>
        </div>
      </div>

      {/* Loading state */}
      {loading && !hasRun && (
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

      {/* Results */}
      {result && (
        <CascadeResults levels={result.levels} allPassed={result.all_passed} />
      )}

      {/* No result */}
      {!loading && !result && (
        <div className="text-center py-12 text-gray-500">
          <div className="mb-4">No verification results available for {entity}</div>
          <button
            onClick={handleRunVerification}
            className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm rounded-md transition-colors"
          >
            Run Verification
          </button>
        </div>
      )}
    </div>
  );
}
