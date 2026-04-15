"use client";

import { useEffect, useState, useCallback } from "react";

const sections = [
  { id: "vision", label: "Tempered Agent" },
  { id: "step-kernel", label: "The Kernel" },
  { id: "step-apps", label: "Apps" },
  { id: "step-proof", label: "Verification" },
  { id: "step-policy", label: "Governance" },
  { id: "step-recorded", label: "Trajectories" },
  { id: "step-growth", label: "Growth" },
  { id: "step-evolution", label: "Evolution" },
  { id: "step-multiagent", label: "Multi-Agent" },
  { id: "roadmap", label: "Roadmap" },
];

export default function ProgressCard() {
  const [visible, setVisible] = useState(false);
  const [activeIdx, setActiveIdx] = useState(-1);
  const [progress, setProgress] = useState(0);

  const update = useCallback(() => {
    const sy = window.scrollY;
    const docH = document.documentElement.scrollHeight - window.innerHeight;
    const pct = docH > 0 ? Math.min((sy / docH) * 100, 100) : 0;
    setProgress(pct);

    // Show after hero
    const hero = document.querySelector("section");
    if (hero) {
      const heroEnd = hero.offsetTop + hero.offsetHeight;
      setVisible(sy > heroEnd - 200);
    }

    // Find active section
    const viewMid = sy + window.innerHeight * 0.35;
    let idx = -1;
    for (let i = sections.length - 1; i >= 0; i--) {
      const el = document.getElementById(sections[i].id);
      if (el && el.offsetTop <= viewMid) {
        idx = i;
        break;
      }
    }
    setActiveIdx(idx);
  }, []);

  useEffect(() => {
    window.addEventListener("scroll", update, { passive: true });
    update();
    return () => window.removeEventListener("scroll", update);
  }, [update]);

  return (
    <aside
      className={`fixed top-20 right-6 z-[999] w-[220px] p-5 bg-[color-mix(in_srgb,var(--color-bg-surface)_92%,transparent)] backdrop-blur-2xl border border-[var(--color-border)] rounded-[3px] transition-all duration-400 max-[1100px]:hidden ${
        visible ? "opacity-100 translate-x-0 pointer-events-auto" : "opacity-0 translate-x-5 pointer-events-none"
      }`}
    >
      <div className="text-[9px] font-bold uppercase tracking-[0.15em] text-[var(--color-text-muted)] mb-3.5">In this page</div>
      <ul className="list-none m-0 p-0 flex flex-col">
        {sections.map((section, i) => (
          <li key={section.id}>
            <button
              onClick={() => {
                const el = document.getElementById(section.id);
                if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
              }}
              className={`text-xs py-1.5 pl-3 border-l-2 w-full text-left cursor-pointer bg-transparent transition-colors duration-200 ${
                i === activeIdx
                  ? "text-[var(--color-accent-teal)] border-[var(--color-accent-teal)] font-semibold"
                  : i < activeIdx
                    ? "text-[var(--color-text-secondary)] border-[var(--color-border-hover)]"
                    : "text-[var(--color-text-muted)] border-transparent hover:text-[var(--color-text-secondary)]"
              }`}
            >
              {section.label}
            </button>
          </li>
        ))}
      </ul>
      <div className="mt-4 pt-3.5 border-t border-[var(--color-border)]">
        <div className="text-[9px] font-bold uppercase tracking-[0.12em] text-[var(--color-text-muted)] mb-2">Progress</div>
        <div className="w-full h-[3px] bg-[var(--color-bg-elevated)] rounded-sm overflow-hidden">
          <div
            className="h-full bg-[var(--color-accent-teal)] rounded-sm transition-[width] duration-400"
            style={{ width: `${progress}%`, transitionTimingFunction: 'cubic-bezier(0.16, 1, 0.3, 1)' }}
          />
        </div>
      </div>
    </aside>
  );
}
