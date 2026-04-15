"use client";

import ScrollReveal from "./ScrollReveal";
import TemperedAgentViz from "./TemperedAgentViz";

export default function VisionSection() {
  return (
    <section id="vision" className="py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <div className="flex gap-[60px] items-center max-[900px]:flex-col max-[900px]:gap-10">
          <div className="flex-1 min-w-0">
            <ScrollReveal>
              <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-[var(--color-accent-teal)] mb-3">
                What&apos;s a Tempered Agent?
              </p>
              <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-serif tracking-[-0.02em] mb-6">
                An agent that accumulates <em className="italic text-[var(--color-accent-teal)]">capabilities.</em>
              </h2>
              <p className="text-base text-[var(--color-text-secondary)] max-w-[520px] mb-4 leading-[1.7]">
                Agent scaffolding — prompt templates, tool wrappers, output
                parsers — shrinks as models get smarter. The infrastructure
                underneath compounds: verified state machines, authorization
                policies, persistent trajectories. A tempered agent builds on
                that layer.
              </p>
              <p className="text-base text-[var(--color-text-secondary)] max-w-[520px] mb-0 leading-[1.7]">
                Everything an agent touches — tools, apps, harnesses — is a
                declarative spec with a signature. The kernel interprets specs
                into running systems. Agents write them, verify them, and
                rewrite them. You hold the approval gate.
              </p>
            </ScrollReveal>
          </div>
          <div className="flex-shrink-0 max-[900px]:w-full max-[900px]:max-w-[380px] max-[900px]:mx-auto">
            <ScrollReveal animation="zoom" delay={100}>
              <div className="w-[320px] max-[900px]:w-full aspect-square bg-[var(--color-bg-elevated)] backdrop-blur-xl border border-[var(--color-border)] rounded-[3px] flex items-center justify-center relative corner-accents">
                <TemperedAgentViz />
              </div>
            </ScrollReveal>
          </div>
        </div>
      </div>
    </section>
  );
}
