"use client";

import { useRef, useState, useEffect } from "react";

const KNOWN_COLORS: Record<string, string> = {
  active: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  done: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  completed: "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  cancelled: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  failed: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  error: "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
};

const HASH_PALETTES = [
  "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
  "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)]",
  "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]",
  "bg-[var(--color-accent-lime-dim)] text-[var(--color-accent-lime)]",
];

function hashString(str: string): number {
  let hash = 0;
  for (let i = 0; i < str.length; i++) {
    hash = (hash << 5) - hash + str.charCodeAt(i);
    hash |= 0;
  }
  return Math.abs(hash);
}

function getStatusColors(status: string): string {
  const lower = status.toLowerCase();
  if (KNOWN_COLORS[lower]) return KNOWN_COLORS[lower];
  return HASH_PALETTES[hashString(lower) % HASH_PALETTES.length];
}

const PINK_STATUSES = new Set(["cancelled", "failed", "error"]);

export default function StatusBadge({ status }: { status: string }) {
  const colors = getStatusColors(status);
  const prevStatusRef = useRef(status);
  const [flashClass, setFlashClass] = useState("");

  useEffect(() => {
    if (prevStatusRef.current !== status) {
      const isPink = PINK_STATUSES.has(status.toLowerCase());
      setFlashClass(isPink ? "animate-flash-pink" : "animate-flash-teal");
      prevStatusRef.current = status;
    }
  }, [status]);

  return (
    <span
      className={`text-xs font-mono px-2 py-0.5 rounded-full transition-colors duration-300 ${colors} ${flashClass}`}
      onAnimationEnd={() => setFlashClass("")}
    >
      {status}
    </span>
  );
}
