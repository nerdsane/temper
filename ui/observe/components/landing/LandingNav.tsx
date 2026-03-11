"use client";

import { useEffect, useState } from "react";
import ThemeToggle from "../ThemeToggle";

export default function LandingNav() {
  const [scrolled, setScrolled] = useState(false);

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 10);
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, []);

  return (
    <nav
      className={`fixed top-0 left-0 right-0 z-[100] backdrop-blur-2xl transition-colors ${
        scrolled ? "bg-[color-mix(in_srgb,var(--color-bg-primary)_80%,transparent)]" : "bg-transparent"
      }`}
    >
      <div className="max-w-[960px] mx-auto px-6 flex items-center justify-between h-[52px]">
        <a
          href="#"
          className="font-bold text-[15px] text-[var(--color-text-primary)] tracking-tight hover:text-[var(--color-text-primary)] no-underline"
          onClick={(e) => {
            e.preventDefault();
            window.scrollTo({ top: 0, behavior: "smooth" });
          }}
        >
          Temper
        </a>
        <ul className="flex items-center gap-7 list-none m-0 p-0">
          <li className="hidden sm:block">
            <a href="#vision" className="text-[13px] text-[var(--color-text-secondary)] font-medium hover:text-[var(--color-text-primary)] no-underline transition-colors">
              Vision
            </a>
          </li>
          <li className="hidden sm:block">
            <a href="#step-kernel" className="text-[13px] text-[var(--color-text-secondary)] font-medium hover:text-[var(--color-text-primary)] no-underline transition-colors">
              How it works
            </a>
          </li>
          <li className="hidden sm:block">
            <a href="#roadmap" className="text-[13px] text-[var(--color-text-secondary)] font-medium hover:text-[var(--color-text-primary)] no-underline transition-colors">
              Roadmap
            </a>
          </li>
          <li>
            <ThemeToggle />
          </li>
          <li>
            <a
              href="https://github.com/nerdsane/temper"
              target="_blank"
              rel="noopener"
              className="inline-flex items-center gap-1.5 px-3 py-1.5 border border-[var(--color-border)] rounded text-[13px] text-[var(--color-text-primary)] hover:border-[var(--color-accent-teal)] hover:bg-[var(--color-accent-teal-dim)] transition-all no-underline"
            >
              <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
                <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
              </svg>
              GitHub
            </a>
          </li>
        </ul>
      </div>
    </nav>
  );
}
