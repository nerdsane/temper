"use client";

import { useState, useCallback } from "react";

export default function MediationViz() {
  const [approved, setApproved] = useState(false);

  const handleApprove = useCallback(() => {
    setApproved(true);
    setTimeout(() => setApproved(false), 3000);
  }, []);

  return (
    <div className="w-full p-8 flex flex-col items-center text-center">
      {/* Shield icon */}
      <div
        className={`w-16 h-16 border-2 rounded-full flex items-center justify-center mb-5 transition-all duration-500 ${
          approved
            ? "border-[var(--color-accent-teal)] shadow-[0_0_30px_var(--color-accent-teal-dim)]"
            : "border-[var(--color-accent-pink)] shadow-[0_0_30px_var(--color-accent-pink-dim)]"
        }`}
      >
        <svg
          width="28"
          height="28"
          viewBox="0 0 24 24"
          fill="none"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
          className={`transition-all duration-500 ${approved ? "stroke-[var(--color-accent-teal)]" : "stroke-[var(--color-accent-pink)]"}`}
        >
          <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" />
        </svg>
      </div>

      {/* Status */}
      <div
        className={`font-mono text-xs font-bold mb-2.5 transition-colors duration-500 ${
          approved ? "text-[var(--color-accent-teal)]" : "text-[var(--color-accent-pink)]"
        }`}
      >
        {approved ? 'APPROVED: Principal::"Agent_1"' : 'DENY: Principal::"Agent_1"'}
      </div>

      <div className="text-[11px] text-[var(--color-text-muted)] mb-5 leading-[1.5]">
        Action &quot;Access_DB&quot; requested.
        <br />
        No permitted policy found.
      </div>

      {/* Approve button */}
      <button
        onClick={handleApprove}
        disabled={approved}
        className="w-full max-w-[200px] py-2.5 rounded text-[11px] font-bold uppercase tracking-[0.1em] cursor-pointer transition-all duration-200 bg-[var(--color-accent-teal-dim)] border border-[var(--color-accent-teal)]/30 text-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal)] hover:text-[var(--color-bg-primary)] disabled:pointer-events-none"
      >
        {approved ? "Policy Generated" : "Approve Access"}
      </button>
    </div>
  );
}
