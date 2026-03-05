"use client";

import { useRef, useState, useEffect } from "react";
import { useRouter } from "next/navigation";
import Link from "next/link";
import type { SpecSummary } from "@/lib/types";

interface SpecCardProps {
  spec: SpecSummary;
}

function VerificationBadge({ spec }: { spec: SpecSummary }) {
  const status = spec.verification_status;
  const levelInfo =
    spec.levels_passed != null && spec.levels_total != null
      ? `${spec.levels_passed}/${spec.levels_total}`
      : null;

  const config: Record<string, { bg: string; text: string; label: string; pulse?: boolean }> = {
    pending: { bg: "bg-zinc-500/10", text: "text-zinc-500", label: "Pending" },
    running: { bg: "bg-yellow-500/10", text: "text-yellow-400", label: "Verifying...", pulse: true },
    passed: { bg: "bg-teal-500/10", text: "text-teal-400", label: "Verified" },
    failed: { bg: "bg-pink-500/10", text: "text-pink-400", label: "Failed" },
    partial: { bg: "bg-yellow-500/10", text: "text-yellow-400", label: levelInfo ? `Partial (${levelInfo})` : "Partial" },
  };

  const c = config[status] ?? config.pending;

  return (
    <span
      className={`text-[10px] font-mono px-2 py-0.5 rounded-full ${c.bg} ${c.text} ${c.pulse ? "animate-pulse" : ""}`}
    >
      {c.label}
    </span>
  );
}

export default function SpecCard({ spec }: SpecCardProps) {
  const router = useRouter();
  const prevVerifyRef = useRef(spec.verification_status);
  const [cardFlash, setCardFlash] = useState("");

  useEffect(() => {
    if (prevVerifyRef.current !== spec.verification_status) {
      setCardFlash(
        spec.verification_status === "passed" ? "animate-flash-teal" :
        spec.verification_status === "failed" ? "animate-flash-pink" : ""
      );
      prevVerifyRef.current = spec.verification_status;
    }
  }, [spec.verification_status]);

  return (
    <Link href={`/specs/${spec.entity_type}`}>
      <div
        className={`bg-[#111115] rounded-lg p-5 hover:bg-white/[0.02] transition-colors cursor-pointer group ${cardFlash}`}
        onAnimationEnd={() => setCardFlash("")}
      >
        <div className="flex items-start justify-between mb-2.5">
          <h3 className="text-base font-semibold text-zinc-100 tracking-tight truncate min-w-0" title={spec.entity_type}>{spec.entity_type}</h3>
          <div className="flex gap-1.5">
            <VerificationBadge spec={spec} />
            <span className="text-[10px] font-mono bg-teal-500/10 text-teal-400 px-2 py-0.5 rounded-full">
              IOA
            </span>
          </div>
        </div>

        <div className="space-y-1.5">
          <div className="flex items-center justify-between text-sm">
            <span className="text-zinc-600">States</span>
            <span className="font-mono text-zinc-400">{spec.states.length}</span>
          </div>
          <div className="flex items-center justify-between text-sm">
            <span className="text-zinc-600">Actions</span>
            <span className="font-mono text-zinc-400">{spec.actions.length}</span>
          </div>
          <div className="flex items-center justify-between text-sm">
            <span className="text-zinc-600">Initial</span>
            <span className="font-mono text-lime-400">{spec.initial_state}</span>
          </div>
        </div>

        <div className="mt-3 flex flex-wrap gap-1">
          {spec.states.map((state) => (
            <span
              key={state}
              className={`text-[10px] px-1.5 py-0.5 rounded font-mono ${
                state === spec.initial_state
                  ? "bg-lime-500/10 text-lime-400"
                  : "bg-white/[0.03] text-zinc-500"
              }`}
            >
              {state}
            </span>
          ))}
        </div>

        <div className="mt-3 flex gap-2">
          <button
            type="button"
            className="text-[11px] text-teal-400 hover:text-teal-300 transition-colors"
            onClick={(e) => {
              e.preventDefault();
              e.stopPropagation();
              router.push(`/verify/${spec.entity_type}`);
            }}
          >
            Verify
          </button>
        </div>
      </div>
    </Link>
  );
}
