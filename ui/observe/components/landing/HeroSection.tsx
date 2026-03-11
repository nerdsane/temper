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

        {/* CTA button */}
        <div className="flex gap-3 justify-center flex-wrap animate-hero-fade-up [animation-delay:0.4s]">
          <a
            href="https://github.com/nerdsane/temper"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 rounded bg-[var(--color-accent-teal)] text-[var(--color-bg-primary)] text-sm font-semibold hover:bg-[var(--color-accent-teal)] hover:-translate-y-px hover:shadow-[0_4px_20px_var(--color-accent-teal-dim)] transition-all no-underline"
          >
            Get Started
          </a>
        </div>

      </div>
    </section>
  );
}
