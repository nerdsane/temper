import type { ReactNode } from "react";

export default function NarrativeSection({
  id,
  step,
  label,
  title,
  paragraphs,
  reversed = false,
  children,
}: {
  id: string;
  step?: string;
  label: string;
  title: string;
  paragraphs: string[];
  reversed?: boolean;
  children: ReactNode;
}) {
  const textContent = (
    <div className="flex-1 min-w-0 py-[25vh] max-[900px]:py-20 max-[900px]:pb-10 max-[900px]:w-full max-[900px]:order-1">
      <span className="text-[11px] font-semibold uppercase tracking-[0.1em] text-[var(--color-accent-teal)] mb-4 block">
        {step ? `Step ${step} — ${label}` : label}
      </span>
      <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-serif tracking-[-0.02em] mb-4">
        {title}
      </h2>
      {paragraphs.map((p, i) => (
        <p key={i} className="text-[15px] text-[var(--color-text-secondary)] mb-6 max-w-[480px] leading-[1.7]">
          {p}
        </p>
      ))}
    </div>
  );

  const vizContent = (
    <div className="flex-1 min-w-0 sticky top-0 h-screen flex items-center justify-center z-10 max-[900px]:relative max-[900px]:h-auto max-[900px]:min-h-[320px] max-[900px]:py-5 max-[900px]:w-full max-[900px]:order-2 max-[900px]:mb-[60px]">
      <div className="w-full max-w-[380px] aspect-square bg-[var(--color-bg-elevated)] backdrop-blur-xl border border-[var(--color-border)] rounded-[3px] relative overflow-hidden flex items-center justify-center transition-[border-color] duration-250 hover:border-[var(--color-border-hover)] max-sm:max-w-full corner-accents">
        {children}
      </div>
    </div>
  );

  return (
    <section className="relative" id={id}>
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <div className="flex gap-[60px] items-start min-h-screen max-[900px]:flex-col max-[900px]:gap-0 max-[900px]:min-h-0">
          {reversed ? (
            <>
              {vizContent}
              {textContent}
            </>
          ) : (
            <>
              {textContent}
              {vizContent}
            </>
          )}
        </div>
      </div>
    </section>
  );
}
