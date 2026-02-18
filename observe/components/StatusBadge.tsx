"use client";

import { useRef, useState, useEffect } from "react";

const KNOWN_COLORS: Record<string, string> = {
  active: "bg-teal-500/15 text-teal-400",
  done: "bg-teal-500/15 text-teal-400",
  completed: "bg-teal-500/15 text-teal-400",
  cancelled: "bg-pink-500/15 text-pink-400",
  failed: "bg-pink-500/15 text-pink-400",
  error: "bg-pink-500/15 text-pink-400",
};

const HASH_PALETTES = [
  "bg-teal-500/15 text-teal-400",
  "bg-yellow-500/15 text-yellow-400",
  "bg-violet-500/15 text-violet-300",
  "bg-lime-500/15 text-lime-400",
  "bg-pink-500/15 text-pink-400",
  "bg-orange-500/15 text-orange-400",
  "bg-cyan-500/15 text-cyan-400",
  "bg-fuchsia-500/15 text-fuchsia-400",
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
