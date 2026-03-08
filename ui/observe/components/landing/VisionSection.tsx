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
              <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-teal-400 mb-3">
                What&apos;s a Tempered Agent?
              </p>
              <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-semibold tracking-[-0.02em] mb-6">
                An agent that accumulates capabilities.
              </h2>
              <p className="text-base text-zinc-400 max-w-[520px] mb-4 leading-[1.7]">
                Most agents start from zero every session. A tempered agent builds
                its own operating environment — task systems, knowledge bases,
                notification pipelines — as verified specs that persist and compose.
              </p>
              <p className="text-base text-zinc-400 max-w-[520px] mb-0 leading-[1.7]">
                Each capability is formally proven before deployment, authorized by
                Cedar policy on every action, and continuously refined through an
                evolution feedback loop. The agent proposes changes to its own
                harness. You hold the gate.
              </p>
            </ScrollReveal>
          </div>
          <div className="flex-shrink-0 max-[900px]:w-full max-[900px]:max-w-[380px] max-[900px]:mx-auto">
            <ScrollReveal animation="zoom" delay={100}>
              <div className="w-[320px] max-[900px]:w-full aspect-square bg-white/[0.02] backdrop-blur-xl border border-white/[0.04] rounded-[3px] flex items-center justify-center">
                <TemperedAgentViz />
              </div>
            </ScrollReveal>
          </div>
        </div>
      </div>
    </section>
  );
}
