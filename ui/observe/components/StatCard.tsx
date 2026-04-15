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
      className={`glass rounded-[2px] px-3 py-2.5 ${className ?? ""}`}
      onAnimationEnd={onAnimationEnd}
    >
      <div className="text-[11px] text-[var(--color-text-secondary)] font-medium">{label}</div>
      <div className={`text-xl font-semibold font-mono mt-0.5 ${color ?? "text-[var(--color-text-primary)]"}`}>
        {value}
      </div>
    </div>
  );
}
