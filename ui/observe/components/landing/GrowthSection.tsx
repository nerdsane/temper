"use client";

import ScrollReveal from "./ScrollReveal";
import GrowthViz from "./GrowthViz";

const stories = [
  {
    trigger: "An agent keeps re-investigating solved bugs.",
    observation:
      "Trajectories show repeated context loss across sessions. The same bugs get researched, diagnosed, and fixed — again and again.",
    action:
      "The evolution engine surfaces the pattern. The agent designs a Knowledge spec — Draft → Indexed → Linked → Archived — with semantic search and Cedar-scoped access.",
    result:
      "You review the reachable states, approve, and the knowledge system hot-reloads. The agent starts retaining what it learns.",
  },
  {
    trigger: "An agent hits a throughput bottleneck.",
    observation:
      "Trajectories show a growing queue of unprocessed work. Single-threaded execution can't keep up with incoming tasks.",
    action:
      "The agent designs a TaskDelegation spec — entities that spawn scoped sub-agents with Cedar permissions narrowed to the delegated task. The spec's invariant guarantees a sub-agent can never escalate beyond its parent's authorization.",
    result:
      "You review, approve, and the agent can now distribute work. Throughput scales without compromising the permission boundary.",
  },
];

export default function GrowthSection() {
  return (
    <section id="step-growth" className="py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-teal-400 mb-3">
            How Agents Grow
          </p>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-semibold tracking-[-0.02em] mb-4">
            Watch an Agent Grow.
          </h2>
          <p className="text-base text-zinc-400 max-w-[580px] mb-16 leading-[1.7]">
            The pattern repeats. Each cycle — trajectory analysis, spec
            proposal, verification, human approval — adds a new capability to
            the agent&apos;s operating environment.
          </p>
        </ScrollReveal>

        <div className="flex gap-12 items-start max-[900px]:flex-col">
          {/* Stories */}
          <div className="flex-1 min-w-0 space-y-16">
            {stories.map((story, i) => (
              <ScrollReveal key={i} animation="fade-up" delay={i * 100}>
                <div className="relative pl-6 border-l border-white/[0.06]">
                  <div className="absolute left-[-5px] top-0 w-[9px] h-[9px] rounded-full bg-teal-400/40 border border-teal-400/60" />
                  <p className="text-[15px] font-semibold text-white mb-3">
                    {story.trigger}
                  </p>
                  <p className="text-[14px] text-zinc-500 mb-3 leading-[1.7]">
                    {story.observation}
                  </p>
                  <p className="text-[14px] text-zinc-400 mb-3 leading-[1.7]">
                    {story.action}
                  </p>
                  <p className="text-[14px] text-teal-400/80 leading-[1.7]">
                    {story.result}
                  </p>
                </div>
              </ScrollReveal>
            ))}
          </div>

          {/* Visualization */}
          <div className="flex-shrink-0 sticky top-[20vh] max-[900px]:relative max-[900px]:top-0 max-[900px]:w-full">
            <div className="w-[340px] max-[900px]:w-full max-[900px]:max-w-[380px] max-[900px]:mx-auto aspect-square bg-white/[0.02] backdrop-blur-xl border border-white/[0.04] rounded-[3px] flex items-center justify-center">
              <GrowthViz />
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
