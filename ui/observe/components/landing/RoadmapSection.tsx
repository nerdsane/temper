import ScrollReveal from "./ScrollReveal";

const cards = [
  {
    tier: "L1-L2",
    tierColor: "text-teal-400",
    title: "Persistent State",
    description: "Verified entities queryable via OData API. Event-sourced truth instead of JSON blobs.",
  },
  {
    tier: "L3-L4",
    tierColor: "text-amber-400",
    title: "WASM Connectivity",
    description: "Streaming-capable integrations as sandboxed modules mediated by Cedar policy.",
  },
  {
    tier: "L5-L6",
    tierColor: "text-zinc-600",
    title: "Agent Execution",
    description: "Headless executors claiming agent entities. Spawning and shared-state orchestration.",
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

        <div className="grid grid-cols-3 gap-4 mt-12 max-[900px]:grid-cols-2 max-sm:grid-cols-1">
          {cards.map((card, i) => (
            <ScrollReveal key={i} animation="zoom" delay={i * 50}>
              <div className="p-7 bg-white/[0.02] backdrop-blur-xl border border-white/[0.04] rounded-[3px] transition-all duration-250 hover:border-white/10 hover:-translate-y-0.5 hover:shadow-[inset_0_1px_0_rgba(255,255,255,0.04),0_4px_24px_rgba(45,212,191,0.04)]">
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
