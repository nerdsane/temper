import ScrollReveal from "./ScrollReveal";

const rows = [
  { label: "Tools", industry: "Stateless, ad-hoc synthesizers", temper: "Verified state machines that persist as specs" },
  { label: "Security", industry: "Hardcoded, manual policies", temper: "Policies derived from behavioral intent" },
  { label: "Growth", industry: "Failures are discarded logs", temper: "Unmet intents feed the evolution engine" },
];

export default function VisionSection() {
  return (
    <section id="vision" className="py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <p className="text-[11px] font-semibold uppercase tracking-[0.1em] text-teal-400 mb-3">The Vision</p>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-semibold tracking-[-0.02em] mb-4">
            Agents Build Tools. Temper Makes Them Safe.
          </h2>
          <p className="text-base text-zinc-400 max-w-[520px] mb-12 leading-[1.7]">
            Agents are starting to synthesize tools at runtime. The infrastructure to make this safe is developing.
            Temper connects formal verification, Cedar authorization, and evolution feedback into one framework.
          </p>

          <table className="w-full border-collapse table-fixed max-md:block">
            <tbody className="max-md:block">
              {rows.map((row, i) => (
                <tr key={i} className="max-md:block max-md:py-4 max-md:border-b max-md:border-white/[0.04]">
                  <td className="py-5 px-4 border-b border-white/[0.04] align-top text-sm font-bold text-white w-[22%] max-md:block max-md:w-full max-md:border-b-0 max-md:py-1 max-md:mb-2 last:border-b-0">
                    {row.label}
                  </td>
                  <td className="py-5 px-4 border-b border-white/[0.04] align-top text-sm text-zinc-400 w-[39%] max-md:block max-md:w-full max-md:border-b-0 max-md:py-1 last:border-b-0">
                    {row.industry}
                  </td>
                  <td className="py-5 px-4 border-b border-white/[0.04] align-top text-sm text-teal-400 font-medium w-[39%] max-md:block max-md:w-full max-md:border-b-0 max-md:py-1 last:border-b-0">
                    {row.temper}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </ScrollReveal>
      </div>
    </section>
  );
}
