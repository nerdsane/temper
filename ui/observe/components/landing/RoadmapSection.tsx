import ScrollReveal from "./ScrollReveal";

const cards = [
  {
    tier: "DONE",
    tierColor: "text-teal-400",
    title: "The Kernel",
    description: "Spec interpreter, verification cascade, Cedar authorization, OData API, event sourcing. 950+ tests.",
  },
  {
    tier: "WORKING",
    tierColor: "text-teal-400",
    title: "Apps as Specs",
    description: "Agents build applications as entity specs — task management, knowledge systems, notification pipelines.",
  },
  {
    tier: "IN PROGRESS",
    tierColor: "text-amber-400",
    title: "Integration Framework",
    description: "Streaming WASM integrations as sandboxed modules. Cedar-mediated connectivity to external services.",
  },
  {
    tier: "PLANNED",
    tierColor: "text-zinc-600",
    title: "Agent Execution",
    description: "Agents as entities with background executors. Spawning children with scoped permissions. The pure tempered agent.",
  },
];

export default function RoadmapSection() {
  return (
    <section id="roadmap" className="py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-teal-400 mb-3">Roadmap</p>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-semibold tracking-[-0.02em] mb-4">Built Bottom-Up.</h2>
          <p className="text-base text-zinc-400 max-w-[520px] mb-12 leading-[1.7]">
            From the kernel to full agent execution.
          </p>
        </ScrollReveal>

        <div className="grid grid-cols-4 gap-4 mt-12 max-[900px]:grid-cols-2 max-sm:grid-cols-1">
          {cards.map((card, i) => (
            <ScrollReveal key={i} animation="zoom" delay={i * 50} className="h-full">
              <div className="h-full p-7 bg-white/[0.02] backdrop-blur-xl border border-white/[0.04] rounded-[3px] transition-all duration-250 hover:border-white/10 hover:-translate-y-0.5 hover:shadow-[inset_0_1px_0_rgba(255,255,255,0.04),0_4px_24px_rgba(45,212,191,0.04)]">
                <span className={`text-[10px] font-semibold ${card.tierColor}`}>{card.tier}</span>
                <h4 className="text-[15px] font-semibold mb-2 mt-1">{card.title}</h4>
                <p className="text-zinc-400 text-[13px] leading-[1.6]">{card.description}</p>
              </div>
            </ScrollReveal>
          ))}
        </div>
      </div>
    </section>
  );
}
