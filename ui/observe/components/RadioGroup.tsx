"use client";

export interface RadioOption<T extends string> {
  value: T;
  label: string;
  description: string;
}

interface RadioGroupProps<T extends string> {
  label: string;
  options: RadioOption<T>[];
  value: T;
  onChange: (v: T) => void;
}

export default function RadioGroup<T extends string>({
  label,
  options,
  value,
  onChange,
}: RadioGroupProps<T>) {
  return (
    <div className="space-y-1.5">
      <div className="text-[10px] text-[var(--color-text-secondary)] uppercase tracking-wider font-medium">
        {label}
      </div>
      <div className="flex flex-wrap gap-1.5">
        {options.map((opt) => (
          <button
            key={opt.value}
            type="button"
            onClick={() => onChange(opt.value)}
            title={opt.description}
            className={`px-2.5 py-1 text-[11px] rounded-sm transition-colors ${
              value === opt.value
                ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] ring-1 ring-[var(--color-accent-teal)]"
                : "bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] hover:bg-[var(--color-bg-elevated)] hover:text-[var(--color-text-secondary)]"
            }`}
          >
            {opt.label}
          </button>
        ))}
      </div>
    </div>
  );
}
