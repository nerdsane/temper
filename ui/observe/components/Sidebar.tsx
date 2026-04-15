"use client";

import { useState, useEffect } from "react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useConnection } from "@/lib/connection";
import { useDecisionNotifier } from "@/lib/decision-notifier";
import { fetchUnmetIntents } from "@/lib/api";
import UserMenu from "@/components/UserMenu";
import ThemeToggle from "@/components/ThemeToggle";

function NavIcon({ icon }: { icon: string }) {
  switch (icon) {
    case "grid":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M4 5a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1H5a1 1 0 01-1-1V5zM14 5a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1h-4a1 1 0 01-1-1V5zM4 15a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1H5a1 1 0 01-1-1v-4zM14 15a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1h-4a1 1 0 01-1-1v-4z" />
        </svg>
      );
    case "workflow":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M13 10V3L4 14h7v7l9-11h-7z" />
        </svg>
      );
    case "shield":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
        </svg>
      );
    case "lock":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M12 15v2m-6 4h12a2 2 0 002-2v-6a2 2 0 00-2-2H6a2 2 0 00-2 2v6a2 2 0 002 2zm10-10V7a4 4 0 00-8 0v4h8z" />
        </svg>
      );
    case "box":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" />
        </svg>
      );
    case "activity":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M22 12h-4l-3 9L9 3l-3 9H2" />
        </svg>
      );
    case "dna":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M4 4v2m0 12v2m16-16v2m0 12v2M7.1 6h9.8M7.1 18h9.8M4.93 8.5h14.14M4.93 15.5h14.14M7.1 11h9.8M7.1 13h9.8" />
        </svg>
      );
    case "users":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0z" />
        </svg>
      );
    case "lightbulb":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9.663 17h4.673M12 3v1m6.364 1.636l-.707.707M21 12h-1M4 12H3m3.343-5.657l-.707-.707m2.828 9.9a5 5 0 117.072 0l-.548.547A3.374 3.374 0 0014 18.469V19a2 2 0 11-4 0v-.531c0-.895-.356-1.754-.988-2.386l-.548-.547z" />
        </svg>
      );
    case "package":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M4 16v1a3 3 0 003 3h10a3 3 0 003-3v-1m-4-4l-4 4m0 0l-4-4m4 4V4" />
        </svg>
      );
    default:
      return null;
  }
}

const navItems = [
  { href: "/dashboard", label: "Dashboard", icon: "grid" },
  { href: "/workflows", label: "Workflows", icon: "workflow" },
  { href: "/activity", label: "Activity", icon: "activity" },
  { href: "/decisions", label: "Decisions", icon: "shield" },
  { href: "/policies", label: "Policies", icon: "lock" },
  { href: "/agents", label: "Agents", icon: "users" },
  { href: "/evolution", label: "Evolution", icon: "dna" },
  { href: "/feature-requests", label: "Feature Requests", icon: "lightbulb" },
  { href: "/integrations", label: "Integrations", icon: "box" },
  { href: "/os-apps", label: "Apps", icon: "package" },
];

export default function Sidebar() {
  const pathname = usePathname();
  const { connected, checking } = useConnection();
  const { pendingCount } = useDecisionNotifier();
  const [unmetCount, setUnmetCount] = useState(0);

  useEffect(() => {
    let mounted = true;
    const poll = async () => {
      try {
        const data = await fetchUnmetIntents();
        if (mounted) setUnmetCount(data.open_count);
      } catch { /* ignore */ }
    };
    poll();
    const interval = setInterval(poll, 30000);
    return () => { mounted = false; clearInterval(interval); };
  }, []);

  const isActive = (href: string) => {
    if (href === "/dashboard") return pathname === "/dashboard";
    return pathname.startsWith(href);
  };

  return (
    <aside className="w-52 bg-[var(--color-bg-primary)]/80 backdrop-blur-xl border-r border-[var(--color-border)] flex flex-col h-screen">
      {/* Logo / Title */}
      <div className="px-4 py-3.5">
        <Link href="/dashboard" className="flex items-center gap-2.5">
          <div>
            <div className="text-[15px] font-bold text-[var(--color-text-primary)] tracking-tight font-display">Temper</div>
            <div className="text-[10px] text-[var(--color-text-muted)] tracking-wide uppercase">Observe</div>
          </div>
        </Link>
      </div>

      {/* Navigation */}
      <nav className="flex-1 px-2 py-2 space-y-0.5 overflow-y-auto" aria-label="Main navigation">
        {navItems.map((item) => (
          <Link
            key={item.href}
            href={item.href}
            className={`flex items-center gap-2.5 px-2.5 py-1.5 rounded-[2px] text-[13px] font-display transition-colors ${
              isActive(item.href)
                ? "text-[var(--color-text-primary)] bg-[var(--color-bg-elevated)]"
                : item.label === "Decisions" && pendingCount > 0
                  ? "text-[var(--color-accent-pink)] bg-[var(--color-accent-pink-dim)] hover:bg-[var(--color-accent-pink-dim)]"
                  : "text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)] hover:bg-[var(--color-bg-elevated)]"
            }`}
          >
            <NavIcon icon={item.icon} />
            {item.label}
            {item.label === "Decisions" && pendingCount > 0 && (
              <span className="ml-auto text-[10px] font-mono bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded-full min-w-[20px] text-center" aria-label={`${pendingCount} pending decisions`}>
                {pendingCount > 99 ? "99+" : pendingCount}
              </span>
            )}
            {item.label === "Evolution" && unmetCount > 0 && (
              <span className="ml-auto text-[10px] font-mono bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] px-1.5 py-0.5 rounded-full min-w-[20px] text-center" aria-label={`${unmetCount} unmet intents`}>
                {unmetCount > 99 ? "99+" : unmetCount}
              </span>
            )}
          </Link>
        ))}
      </nav>

      {/* User */}
      <div className="border-t border-[var(--color-border)]">
        <UserMenu />
      </div>

      {/* Footer */}
      <div className="px-4 py-3 border-t border-[var(--color-border)] flex items-center justify-between">
        <div className="text-[10px] text-[var(--color-text-muted)] font-mono">TEMPER v0.1.0</div>
        <div className="flex items-center gap-2">
          <ThemeToggle />
          <div className="flex items-center gap-1.5" aria-label={checking ? "Checking connection" : connected ? "Connected" : "Disconnected"}>
            <div className={`w-1.5 h-1.5 rounded-full ${checking ? "bg-[var(--color-text-muted)]" : connected ? "bg-[var(--color-accent-teal)]" : "bg-[var(--color-accent-pink)]"}`} />
            <span className={`text-[10px] font-mono ${checking ? "text-[var(--color-text-muted)]" : connected ? "text-[var(--color-text-muted)]" : "text-[var(--color-accent-pink)]"}`}>
              {checking ? "..." : connected ? "" : "offline"}
            </span>
          </div>
        </div>
      </div>
    </aside>
  );
}
