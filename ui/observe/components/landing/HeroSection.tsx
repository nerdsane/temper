"use client";

/* eslint-disable @next/next/no-img-element */

export default function HeroSection() {
  return (
    <section className="pt-40 pb-36 text-center relative max-sm:pt-36 max-sm:pb-24">
      {/* Hero glow */}
      <div className="absolute top-[60px] left-1/2 -translate-x-1/2 w-[600px] h-[600px] bg-[radial-gradient(circle,rgba(45,212,191,0.08)_0%,transparent_60%)] pointer-events-none animate-glow-pulse" />

      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        {/* Mascot */}
        <div className="mb-8 animate-hero-mascot">
          <picture>
            <source media="(prefers-color-scheme: light)" srcSet="/assets/mascot-vectorized.svg" />
            <img
              src="/assets/mascot-vectorized-light.svg"
              width={200}
              height={200}
              alt="Temper mascot"
              className="w-[200px] h-auto max-sm:w-[140px] mx-auto"
            />
          </picture>
        </div>

        {/* Heading */}
        <h1 className="text-[clamp(2.5rem,5.5vw,4rem)] font-bold tracking-[-0.04em] leading-[1.1] max-w-[680px] mx-auto mb-6 bg-gradient-to-br from-white to-teal-400 bg-clip-text text-transparent animate-hero-fade-up [animation-delay:0.2s]">
          The framework where agents build their own OS.
        </h1>

        {/* Tagline */}
        <p className="text-lg text-zinc-400 max-w-[540px] mx-auto mb-12 leading-[1.7] animate-hero-fade-up [animation-delay:0.3s]">
          A tempered agent accumulates capabilities. It describes what it needs, the kernel proves it correct, and every action feeds back into evolution.
        </p>

        {/* CTA buttons */}
        <div className="flex gap-3 justify-center flex-wrap animate-hero-fade-up [animation-delay:0.4s]">
          <a
            href="https://github.com/nerdsane/temper"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 rounded bg-teal-400 text-[#0a0a0c] text-sm font-semibold hover:bg-teal-300 hover:-translate-y-px hover:shadow-[0_4px_20px_rgba(45,212,191,0.08)] transition-all no-underline"
          >
            Get Started
          </a>
          <a
            href="https://github.com/nerdsane/temper/blob/main/docs/PAPER.md"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 rounded bg-white/[0.02] text-white text-sm font-semibold border border-white/[0.04] hover:bg-white/[0.04] hover:border-white/10 hover:-translate-y-px transition-all no-underline"
          >
            Read the Paper
          </a>
        </div>

        {/* Stats */}
        <div className="flex flex-wrap gap-10 justify-center mt-12 animate-hero-fade-up [animation-delay:0.5s]">
          <div className="flex flex-col gap-2 items-center">
            <span className="text-[28px] font-bold font-mono text-white tracking-[-0.04em]">L0–L3</span>
            <span className="text-[10px] uppercase tracking-[0.2em] text-zinc-600 font-bold">Proof Cascade</span>
          </div>
          <div className="flex flex-col gap-2 items-center">
            <span className="text-[28px] font-bold font-mono text-white tracking-[-0.04em]">Cedar</span>
            <span className="text-[10px] uppercase tracking-[0.2em] text-zinc-600 font-bold">Policy Engine</span>
          </div>
          <div className="flex flex-col gap-2 items-center">
            <span className="text-[28px] font-bold font-mono text-white tracking-[-0.04em]">WASM</span>
            <span className="text-[10px] uppercase tracking-[0.2em] text-zinc-600 font-bold">Sandboxed Integrations</span>
          </div>
        </div>
      </div>
    </section>
  );
}
