"use client";

/* eslint-disable @next/next/no-img-element */

export default function HeroSection() {
  return (
    <section className="pt-40 pb-36 text-center relative max-sm:pt-36 max-sm:pb-24">
      {/* Hero glow */}
      <div className="absolute top-[60px] left-1/2 -translate-x-1/2 w-[600px] h-[600px] pointer-events-none animate-glow-pulse" style={{ background: 'radial-gradient(circle, color-mix(in srgb, var(--color-accent-teal) 8%, transparent) 0%, transparent 60%)' }} />

      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        {/* Mascot — light version on dark bg, dark version on light bg */}
        <div className="mb-8 animate-hero-mascot">
          <img
            src="/assets/mascot-vectorized-light.svg"
            width={140}
            height={140}
            alt="Temper mascot"
            className="w-[140px] h-auto max-sm:w-[98px] mx-auto mascot-for-dark"
          />
          <img
            src="/assets/mascot-vectorized.svg"
            width={140}
            height={140}
            alt="Temper mascot"
            className="w-[140px] h-auto max-sm:w-[98px] mx-auto mascot-for-light"
          />
        </div>

        {/* Heading */}
        <h1 className="text-[clamp(2.5rem,5.5vw,3.625rem)] font-serif tracking-[-0.015em] leading-[1.12] max-w-[680px] mx-auto mb-6 text-[var(--color-text-primary)] animate-hero-fade-up [animation-delay:0.2s]">
          Where agents{" "}<br className="max-sm:hidden" />
          <em className="italic text-[var(--color-accent-teal)]">build their own</em> OS and tools.
        </h1>

        {/* Tagline */}
        <p className="text-lg text-[var(--color-text-secondary)] max-w-[540px] mx-auto mb-12 leading-[1.7] animate-hero-fade-up [animation-delay:0.3s]">
          A tempered agent accumulates capabilities. It describes what it needs, the kernel proves it correct, and every action feeds back into evolution.
        </p>

        {/* CTA buttons */}
        <div className="flex gap-3 justify-center flex-wrap animate-hero-fade-up [animation-delay:0.4s]">
          <span className="inline-flex items-center gap-2 px-[22px] py-2.5 rounded bg-[var(--color-accent-teal)] text-[var(--color-bg-primary)] text-sm font-semibold cursor-default">
            Coming Soon
          </span>
          <a
            href="https://github.com/nerdsane/temper"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 rounded bg-[var(--color-bg-elevated)] text-[var(--color-text-primary)] text-sm font-semibold border border-[var(--color-border)] hover:border-[var(--color-border-hover)] hover:-translate-y-px transition-all no-underline"
          >
            <svg className="w-4 h-4" viewBox="0 0 24 24" fill="currentColor"><path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z"/></svg>
            GitHub
          </a>
        </div>

      </div>
    </section>
  );
}
