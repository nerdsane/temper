"use client";

import { useEffect, useState } from "react";
import { useParams } from "next/navigation";
import Link from "next/link";
import { fetchSpecDetail } from "@/lib/api";
import type { SpecDetail } from "@/lib/mock-data";
import StateMachineGraph from "@/components/StateMachineGraph";

export default function SpecViewer() {
  const params = useParams();
  const entity = params.entity as string;
  const [spec, setSpec] = useState<SpecDetail | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    async function load() {
      const data = await fetchSpecDetail(entity);
      setSpec(data);
      setLoading(false);
    }
    load();
  }, [entity]);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500 text-sm">Loading spec...</div>
      </div>
    );
  }

  if (!spec) {
    return (
      <div className="text-center py-12">
        <div className="text-gray-500 mb-4">Spec not found: {entity}</div>
        <Link href="/" className="text-blue-400 hover:text-blue-300 text-sm">
          Back to Dashboard
        </Link>
      </div>
    );
  }

  return (
    <div>
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <div className="flex items-center gap-2 text-sm text-gray-500 mb-1">
            <Link href="/" className="hover:text-gray-300">Dashboard</Link>
            <span>/</span>
            <span className="text-gray-300">Specs</span>
            <span>/</span>
            <span className="text-gray-300">{spec.entity_type}</span>
          </div>
          <h1 className="text-2xl font-bold text-gray-100">{spec.entity_type} Spec</h1>
        </div>
        <Link
          href={`/verify/${spec.entity_type}`}
          className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white text-sm rounded-md transition-colors"
        >
          Run Verification
        </Link>
      </div>

      {/* State Machine Diagram */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-3">State Machine</h2>
        <StateMachineGraph spec={spec} />
        <div className="flex gap-6 mt-3 text-xs text-gray-500">
          <div className="flex items-center gap-2">
            <div className="w-3 h-3 rounded border-2 border-green-500 bg-green-950" />
            <span>Initial state</span>
          </div>
          <div className="flex items-center gap-2">
            <div className="w-3 h-3 rounded border-2 border-gray-600 bg-gray-900" />
            <span>Normal state</span>
          </div>
          <div className="flex items-center gap-2">
            <div className="w-3 h-3 rounded border-2 border-dashed border-gray-500 bg-gray-900" />
            <span>Terminal state</span>
          </div>
        </div>
      </div>

      {/* Transition Table */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-3">Transitions</h2>
        <div className="bg-gray-900 border border-gray-800 rounded-lg overflow-hidden">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800">
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Action</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Kind</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">From</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">To</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Guard</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Effect</th>
              </tr>
            </thead>
            <tbody>
              {spec.actions.map((action, i) => (
                <tr
                  key={i}
                  className="border-b border-gray-800/50 hover:bg-gray-800/30 transition-colors"
                >
                  <td className="px-4 py-2.5 font-mono text-blue-400">{action.name}</td>
                  <td className="px-4 py-2.5">
                    <span
                      className={`text-xs font-mono px-1.5 py-0.5 rounded ${
                        action.kind === "input"
                          ? "bg-blue-900/40 text-blue-400"
                          : "bg-purple-900/40 text-purple-400"
                      }`}
                    >
                      {action.kind}
                    </span>
                  </td>
                  <td className="px-4 py-2.5 font-mono text-gray-300">{action.from}</td>
                  <td className="px-4 py-2.5 font-mono text-gray-300">{action.to}</td>
                  <td className="px-4 py-2.5 font-mono text-yellow-400/80 text-xs">
                    {action.guard || <span className="text-gray-600">--</span>}
                  </td>
                  <td className="px-4 py-2.5 font-mono text-gray-400 text-xs">
                    {action.effect || <span className="text-gray-600">--</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Invariants */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-3">Invariants</h2>
        <div className="space-y-2">
          {spec.invariants.map((inv, i) => (
            <div
              key={i}
              className="bg-gray-900 border border-gray-800 rounded-lg p-4"
            >
              <div className="flex items-center gap-2 mb-2">
                <span className="text-sm font-semibold text-gray-200">{inv.name}</span>
              </div>
              <div className="text-sm space-y-1">
                <div className="flex gap-2">
                  <span className="text-gray-500 w-16 flex-shrink-0">when</span>
                  <code className="font-mono text-yellow-400/80 text-xs">{inv.when}</code>
                </div>
                <div className="flex gap-2">
                  <span className="text-gray-500 w-16 flex-shrink-0">assert</span>
                  <code className="font-mono text-green-400/80 text-xs">{inv.assertion}</code>
                </div>
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* State Variables */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-3">State Variables</h2>
        <div className="bg-gray-900 border border-gray-800 rounded-lg overflow-hidden">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800">
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Name</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Type</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Initial Value</th>
              </tr>
            </thead>
            <tbody>
              {spec.state_variables.map((v, i) => (
                <tr
                  key={i}
                  className="border-b border-gray-800/50 hover:bg-gray-800/30 transition-colors"
                >
                  <td className="px-4 py-2.5 font-mono text-gray-200">{v.name}</td>
                  <td className="px-4 py-2.5 font-mono text-purple-400 text-xs">{v.var_type}</td>
                  <td className="px-4 py-2.5 font-mono text-gray-400 text-xs">{v.initial}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
