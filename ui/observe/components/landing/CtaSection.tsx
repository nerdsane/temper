import ScrollReveal from "./ScrollReveal";

export default function CtaSection() {
  return (
    <section className="text-center py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-serif tracking-[-0.02em] mb-12">
            The living harness is <em className="italic text-[var(--color-accent-teal)]">coming.</em>
          </h2>
          <a
            href="https://github.com/nerdsane/temper"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 bg-[var(--color-accent-teal)] text-[var(--color-bg-primary)] rounded font-semibold text-sm no-underline transition-all hover:bg-[var(--color-accent-teal)] hover:-translate-y-px hover:shadow-[0_4px_20px_var(--color-accent-teal-dim)]"
          >
            Get Started on GitHub
            <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
              <path d="M5 12h14M12 5l7 7-7 7" />
            </svg>
          </a>
        </ScrollReveal>
      </div>
    </section>
  );
}
