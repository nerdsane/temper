import Link from "next/link";
import type { SpecSummary } from "@/lib/types";

interface SpecCardProps {
  spec: SpecSummary;
}

export default function SpecCard({ spec }: SpecCardProps) {
  return (
    <Link href={`/specs/${spec.entity_type}`}>
      <div className="bg-gray-900 border border-gray-800 rounded-lg p-5 hover:border-gray-600 transition-colors cursor-pointer">
        <div className="flex items-start justify-between mb-3">
          <h3 className="text-lg font-semibold text-gray-100">{spec.entity_type}</h3>
          <span className="text-xs font-mono bg-blue-900/50 text-blue-400 px-2 py-0.5 rounded">
            IOA
          </span>
        </div>

        <div className="space-y-2">
          <div className="flex items-center justify-between text-sm">
            <span className="text-gray-500">States</span>
            <span className="font-mono text-gray-300">{spec.states.length}</span>
          </div>
          <div className="flex items-center justify-between text-sm">
            <span className="text-gray-500">Actions</span>
            <span className="font-mono text-gray-300">{spec.actions.length}</span>
          </div>
          <div className="flex items-center justify-between text-sm">
            <span className="text-gray-500">Initial</span>
            <span className="font-mono text-green-400">{spec.initial_state}</span>
          </div>
        </div>

        <div className="mt-4 flex flex-wrap gap-1.5">
          {spec.states.map((state) => (
            <span
              key={state}
              className={`text-xs px-2 py-0.5 rounded font-mono ${
                state === spec.initial_state
                  ? "bg-green-900/40 text-green-400 border border-green-800"
                  : "bg-gray-800 text-gray-400 border border-gray-700"
              }`}
            >
              {state}
            </span>
          ))}
        </div>

        <div className="mt-4 flex gap-2">
          <Link
            href={`/verify/${spec.entity_type}`}
            className="text-xs text-blue-400 hover:text-blue-300"
            onClick={(e) => e.stopPropagation()}
          >
            Verify
          </Link>
        </div>
      </div>
    </Link>
  );
}
