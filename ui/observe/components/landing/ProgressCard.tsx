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
      className={`fixed top-20 right-6 z-[999] w-[220px] p-5 bg-[rgba(17,17,21,0.92)] backdrop-blur-2xl border border-white/[0.04] rounded-[3px] transition-all duration-400 max-[1100px]:hidden ${
        visible ? "opacity-100 translate-x-0 pointer-events-auto" : "opacity-0 translate-x-5 pointer-events-none"
      }`}
    >
      <div className="text-[9px] font-bold uppercase tracking-[0.15em] text-zinc-600 mb-3.5">In this page</div>
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
                  ? "text-teal-400 border-teal-400 font-semibold"
                  : i < activeIdx
                    ? "text-zinc-400 border-white/[0.08]"
                    : "text-zinc-600 border-transparent hover:text-zinc-400"
              }`}
            >
              {section.label}
            </button>
          </li>
        ))}
      </ul>
      <div className="mt-4 pt-3.5 border-t border-white/[0.04]">
        <div className="text-[9px] font-bold uppercase tracking-[0.12em] text-zinc-600 mb-2">Progress</div>
        <div className="w-full h-[3px] bg-white/[0.04] rounded-sm overflow-hidden">
          <div
            className="h-full bg-teal-400 rounded-sm transition-[width] duration-400 ease-out"
            style={{ width: `${progress}%` }}
          />
        </div>
      </div>
    </aside>
  );
}
