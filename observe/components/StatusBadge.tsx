const KNOWN_COLORS: Record<string, string> = {
  active: "bg-green-900/40 text-green-400 border-green-800",
  done: "bg-green-900/40 text-green-400 border-green-800",
  completed: "bg-green-900/40 text-green-400 border-green-800",
  cancelled: "bg-red-900/40 text-red-400 border-red-800",
  failed: "bg-red-900/40 text-red-400 border-red-800",
  error: "bg-red-900/40 text-red-400 border-red-800",
};

const HASH_PALETTES = [
  "bg-blue-900/40 text-blue-400 border-blue-800",
  "bg-yellow-900/40 text-yellow-400 border-yellow-800",
  "bg-purple-900/40 text-purple-400 border-purple-800",
  "bg-cyan-900/40 text-cyan-400 border-cyan-800",
  "bg-orange-900/40 text-orange-400 border-orange-800",
  "bg-pink-900/40 text-pink-400 border-pink-800",
  "bg-teal-900/40 text-teal-400 border-teal-800",
  "bg-indigo-900/40 text-indigo-400 border-indigo-800",
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

export default function StatusBadge({ status }: { status: string }) {
  const colors = getStatusColors(status);

  return (
    <span className={`text-xs font-mono px-2 py-0.5 rounded border ${colors}`}>
      {status}
    </span>
  );
}
