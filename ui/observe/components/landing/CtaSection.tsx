import ScrollReveal from "./ScrollReveal";

export default function CtaSection() {
  return (
    <section className="text-center py-[120px] max-sm:py-20">
      <div className="max-w-[960px] mx-auto px-6 relative z-[2]">
        <ScrollReveal>
          <h2 className="text-[clamp(1.5rem,3vw,2rem)] font-semibold tracking-[-0.02em] mb-12">
            The living harness is coming.
          </h2>
          <a
            href="https://github.com/nerdsane/temper"
            target="_blank"
            rel="noopener"
            className="inline-flex items-center gap-2 px-[22px] py-2.5 bg-teal-400 text-[#0a0a0c] rounded font-semibold text-sm no-underline transition-all hover:bg-teal-300 hover:-translate-y-px hover:shadow-[0_4px_20px_rgba(45,212,191,0.08)]"
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
