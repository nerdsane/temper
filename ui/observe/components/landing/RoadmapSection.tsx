import ScrollReveal from "./ScrollReveal";

const cards = [
  {
    tier: "DONE",
    tierColor: "text-[var(--color-accent-teal)]",
    title: "The Kernel",
    description: "Spec interpreter, verification cascade, Cedar authorization, OData API, event sourcing. 950+ tests.",
  },
  {
    tier: "WORKING",
    tierColor: "text-[var(--color-accent-teal)]",
    title: "Apps as Specs",
    description: "Agents build applications as entity specs — task management, knowledge systems, notification pipelines.",
  },
  {
    tier: "IN PROGRESS",
    tierColor: "text-[var(--color-accent-pink)]",
    title: "Integration Framework",
    description: "Streaming WASM integrations as sandboxed modules. Cedar-mediated connectivity to external services.",
  },
  {
    tier: "PLANNED",
    tierColor: "text-[var(--color-text-muted)]",
    title: "Agent Execution",
    description: "Agents as entities with background executors. Spawning children with scoped permissions. The pure tempered agent.",
  },
];

export default function RoadmapSection() {
  return (
    <section id="roadmap" className="py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-[var(--color-accent-teal)] mb-3">Roadmap</p>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-serif tracking-[-0.02em] mb-4">Built <em className="italic text-[var(--color-accent-teal)]">Bottom-Up.</em></h2>
          <p className="text-base text-[var(--color-text-secondary)] max-w-[520px] mb-12 leading-[1.7]">
            From the kernel to full agent execution.
          </p>
        </ScrollReveal>

        <div className="grid grid-cols-4 gap-4 mt-12 max-[900px]:grid-cols-2 max-sm:grid-cols-1">
          {cards.map((card, i) => (
            <ScrollReveal key={i} animation="zoom" delay={i * 50} className="h-full">
              <div className="h-full p-7 bg-[var(--color-bg-elevated)] backdrop-blur-xl border border-[var(--color-border)] rounded-[3px] transition-all duration-250 hover:border-[var(--color-border-hover)] hover:-translate-y-0.5 hover:shadow-[inset_0_1px_0_var(--color-bg-elevated),0_4px_24px_var(--color-accent-teal-dim)] relative corner-accents">
                <span className={`text-[10px] font-semibold ${card.tierColor}`}>{card.tier}</span>
                <h4 className="text-[15px] font-semibold mb-2 mt-1">{card.title}</h4>
                <p className="text-[var(--color-text-secondary)] text-[13px] leading-[1.6]">{card.description}</p>
              </div>
            </ScrollReveal>
          ))}
        </div>
      </div>
    </section>
  );
}
