"use client";

import { useEffect, useState } from "react";
import { fetchSpecs, fetchEntities } from "@/lib/api";
import type { SpecSummary, EntitySummary } from "@/lib/mock-data";
import SpecCard from "@/components/SpecCard";
import Link from "next/link";

export default function Dashboard() {
  const [specs, setSpecs] = useState<SpecSummary[]>([]);
  const [entities, setEntities] = useState<EntitySummary[]>([]);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    async function load() {
      const [specData, entityData] = await Promise.all([
        fetchSpecs(),
        fetchEntities(),
      ]);
      setSpecs(specData);
      setEntities(entityData);
      setLoading(false);
    }
    load();
  }, []);

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-gray-500 text-sm">Loading...</div>
      </div>
    );
  }

  // Group entities by type
  const entityCounts = entities.reduce<Record<string, number>>((acc, e) => {
    acc[e.entity_type] = (acc[e.entity_type] || 0) + 1;
    return acc;
  }, {});

  return (
    <div>
      {/* Header */}
      <div className="mb-8">
        <h1 className="text-2xl font-bold text-gray-100">Dashboard</h1>
        <p className="text-sm text-gray-500 mt-1">
          Overview of loaded specs and active entities
        </p>
      </div>

      {/* Stats row */}
      <div className="grid grid-cols-3 gap-4 mb-8">
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Loaded Specs</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {specs.length}
          </div>
        </div>
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Active Entities</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {entities.length}
          </div>
        </div>
        <div className="bg-gray-900 border border-gray-800 rounded-lg p-4">
          <div className="text-sm text-gray-500">Entity Types</div>
          <div className="text-3xl font-bold font-mono text-gray-100 mt-1">
            {Object.keys(entityCounts).length}
          </div>
        </div>
      </div>

      {/* Spec cards */}
      <div className="mb-8">
        <h2 className="text-lg font-semibold text-gray-200 mb-4">Specs</h2>
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
          {specs.map((spec) => (
            <SpecCard key={spec.entity_type} spec={spec} />
          ))}
        </div>
      </div>

      {/* Entity list */}
      <div>
        <h2 className="text-lg font-semibold text-gray-200 mb-4">Entities</h2>
        <div className="bg-gray-900 border border-gray-800 rounded-lg overflow-hidden">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-gray-800">
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Type</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">ID</th>
                <th className="text-left px-4 py-3 text-gray-500 font-medium">Status</th>
                <th className="text-right px-4 py-3 text-gray-500 font-medium"></th>
              </tr>
            </thead>
            <tbody>
              {entities.map((entity) => (
                <tr
                  key={`${entity.entity_type}-${entity.entity_id}`}
                  className="border-b border-gray-800/50 hover:bg-gray-800/30 transition-colors"
                >
                  <td className="px-4 py-3 font-mono text-gray-300">
                    {entity.entity_type}
                  </td>
                  <td className="px-4 py-3 font-mono text-gray-400">
                    {entity.entity_id}
                  </td>
                  <td className="px-4 py-3">
                    <StatusBadge status={entity.current_state ?? entity.actor_status} />
                  </td>
                  <td className="px-4 py-3 text-right">
                    <Link
                      href={`/entities/${entity.entity_type}/${entity.entity_id}`}
                      className="text-blue-400 hover:text-blue-300 text-xs"
                    >
                      Inspect
                    </Link>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}

function StatusBadge({ status }: { status: string }) {
  const colorMap: Record<string, string> = {
    Open: "bg-blue-900/40 text-blue-400 border-blue-800",
    InProgress: "bg-yellow-900/40 text-yellow-400 border-yellow-800",
    Review: "bg-purple-900/40 text-purple-400 border-purple-800",
    Closed: "bg-gray-800 text-gray-400 border-gray-700",
    Cancelled: "bg-red-900/40 text-red-400 border-red-800",
    Draft: "bg-gray-800 text-gray-400 border-gray-700",
    Sent: "bg-blue-900/40 text-blue-400 border-blue-800",
    Paid: "bg-green-900/40 text-green-400 border-green-800",
    Overdue: "bg-red-900/40 text-red-400 border-red-800",
    Voided: "bg-gray-800 text-gray-500 border-gray-700",
    Pending: "bg-yellow-900/40 text-yellow-400 border-yellow-800",
    Confirmed: "bg-blue-900/40 text-blue-400 border-blue-800",
    Shipped: "bg-purple-900/40 text-purple-400 border-purple-800",
    Delivered: "bg-green-900/40 text-green-400 border-green-800",
    Returned: "bg-red-900/40 text-red-400 border-red-800",
  };

  const colors = colorMap[status] || "bg-gray-800 text-gray-400 border-gray-700";

  return (
    <span className={`text-xs font-mono px-2 py-0.5 rounded border ${colors}`}>
      {status}
    </span>
  );
}
