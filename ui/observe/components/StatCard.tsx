"use client";

interface StatCardProps {
  label: string;
  value: string | number;
  color?: string;
  className?: string;
  onAnimationEnd?: () => void;
}

export default function StatCard({ label, value, color, className, onAnimationEnd }: StatCardProps) {
  return (
    <div
      className={`glass rounded-md px-3 py-2.5 ${className ?? ""}`}
      onAnimationEnd={onAnimationEnd}
    >
      <div className="text-[11px] text-zinc-500 font-medium">{label}</div>
      <div className={`text-xl font-semibold font-mono mt-0.5 ${color ?? "text-zinc-100"}`}>
        {value}
      </div>
    </div>
  );
}
