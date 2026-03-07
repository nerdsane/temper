"use client";

export default function StatCard({
  label,
  value,
  color,
}: {
  label: string;
  value: string | number;
  color?: string;
}) {
  return (
    <div className="glass rounded p-3.5">
      <div className="text-[12px] text-zinc-600">{label}</div>
      <div
        className={`text-4xl font-bold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}
      >
        {value}
      </div>
    </div>
  );
}
