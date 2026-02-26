"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState, useMemo, useCallback } from "react";
import { fetchSpecs, fetchEntities } from "@/lib/api";
import { useConnection } from "@/lib/connection";
import { useDecisionNotifier } from "@/lib/decision-notifier";
import type { SpecSummary } from "@/lib/types";

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
    case "file-text":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
        </svg>
      );
    case "shield":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
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
    case "eye":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M15 12a3 3 0 11-6 0 3 3 0 016 0z" />
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M2.458 12C3.732 7.943 7.523 5 12 5c4.478 0 8.268 2.943 9.542 7-1.274 4.057-5.064 7-9.542 7-4.477 0-8.268-2.943-9.542-7z" />
        </svg>
      );
    case "users":
      return (
        <svg className="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M17 20h5v-2a3 3 0 00-5.356-1.857M17 20H7m10 0v-2c0-.656-.126-1.283-.356-1.857M7 20H2v-2a3 3 0 015.356-1.857M7 20v-2c0-.656.126-1.283.356-1.857m0 0a5.002 5.002 0 019.288 0M15 7a3 3 0 11-6 0 3 3 0 016 0z" />
        </svg>
      );
    default:
      return null;
  }
}

function CollapsibleSection({
  title,
  storageKey,
  children,
  count,
}: {
  title: string;
  storageKey: string;
  children: React.ReactNode;
  count?: number;
}) {
  const [isOpen, setIsOpen] = useState(() => {
    if (typeof window === "undefined") return true;
    const stored = localStorage.getItem(`sidebar-${storageKey}`);
    return stored === null ? true : stored === "true";
  });

  const toggle = useCallback(() => {
    setIsOpen((prev) => {
      const next = !prev;
      localStorage.setItem(`sidebar-${storageKey}`, String(next));
      return next;
    });
  }, [storageKey]);

  return (
    <div className="mt-3">
      <button
        onClick={toggle}
        className="flex items-center gap-1.5 px-2.5 py-1 w-full text-left text-[10px] font-medium text-zinc-600 uppercase tracking-widest hover:text-zinc-400 transition-colors group"
      >
        <svg
          className={`w-3 h-3 text-zinc-700 transition-transform duration-150 ${isOpen ? "rotate-90" : ""}`}
          fill="none"
          stroke="currentColor"
          viewBox="0 0 24 24"
        >
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
        </svg>
        <span>{title}</span>
        {count !== undefined && (
          <span className="text-zinc-700 group-hover:text-zinc-500">{count}</span>
        )}
      </button>
      {isOpen && children}
    </div>
  );
}

export default function Sidebar() {
  const pathname = usePathname();
  const { connected, checking } = useConnection();
  const { pendingCount } = useDecisionNotifier();
  const [specs, setSpecs] = useState<SpecSummary[]>([]);
  const [entityCounts, setEntityCounts] = useState<Record<string, number>>({});

  useEffect(() => {
    async function loadNav() {
      try {
        const [specData, entityData] = await Promise.all([
          fetchSpecs(),
          fetchEntities(),
        ]);
        setSpecs(specData);
        const counts: Record<string, number> = {};
        for (const e of entityData) {
          counts[e.entity_type] = (counts[e.entity_type] || 0) + 1;
        }
        setEntityCounts(counts);
      } catch {
        // Sidebar nav gracefully falls back to static items
      }
    }
    loadNav();
  }, []);

  const isActive = (href: string) => {
    if (href === "/") return pathname === "/";
    return pathname.startsWith(href.split("/").slice(0, 3).join("/"));
  };

  const staticItems = [
    { href: "/", label: "Dashboard", icon: "grid" },
    { href: "/visualize", label: "Visualization", icon: "eye" },
    { href: "/workflows", label: "Workflows", icon: "workflow" },
    { href: "/activity", label: "Activity", icon: "activity" },
    { href: "/decisions", label: "Decisions", icon: "shield" },
    { href: "/agents", label: "Agents", icon: "users" },
    { href: "/evolution", label: "Evolution", icon: "dna" },
    { href: "/integrations", label: "Integrations", icon: "box" },
  ];

  // Group specs by tenant/app (hide internal platform tenant)
  const specsByTenant = useMemo(() => {
    const groups: Record<string, SpecSummary[]> = {};
    for (const spec of specs) {
      const tenant = spec.tenant || "default";
      if (tenant === "temper-system") continue;
      if (!groups[tenant]) groups[tenant] = [];
      groups[tenant].push(spec);
    }
    return groups;
  }, [specs]);

  return (
    <aside className="w-52 bg-[#0a0a0c]/80 backdrop-blur-xl border-r border-white/[0.06] flex flex-col h-screen">
      {/* Logo / Title */}
      <div className="px-4 py-3.5">
        <Link href="/" className="flex items-center gap-2.5">
          <div>
            <div className="text-[15px] font-bold text-zinc-100 tracking-tight font-display">Temper</div>
            <div className="text-[10px] text-zinc-600 tracking-wide uppercase">Observe</div>
          </div>
        </Link>
      </div>

      {/* Navigation */}
      <nav className="flex-1 px-2 py-2 space-y-0.5 overflow-y-auto">
        {/* Static nav */}
        {staticItems.map((item) => (
          <Link
            key={item.href}
            href={item.href}
            className={`flex items-center gap-2.5 px-2.5 py-1.5 rounded-sm text-[13px] font-display transition-colors ${
              isActive(item.href)
                ? "text-zinc-100 bg-white/[0.06]"
                : "text-zinc-500 hover:text-zinc-300 hover:bg-white/[0.04]"
            }`}
          >
            <NavIcon icon={item.icon} />
            {item.label}
            {item.label === "Decisions" && pendingCount > 0 && (
              <span className="ml-auto text-[10px] font-mono bg-pink-500/20 text-pink-400 px-1.5 py-0.5 rounded-full min-w-[20px] text-center">
                {pendingCount}
              </span>
            )}
          </Link>
        ))}

        {/* Dynamic spec nav grouped by app/tenant */}
        {Object.entries(specsByTenant).map(([tenant, tenantSpecs]) => (
          <CollapsibleSection
            key={tenant}
            title={tenant}
            storageKey={`tenant-${tenant}`}
            count={tenantSpecs.length}
          >
            {tenantSpecs.map((spec) => (
              <Link
                key={`${tenant}:${spec.entity_type}`}
                href={`/specs/${spec.entity_type}`}
                className={`flex items-center justify-between px-2.5 py-1.5 rounded-sm text-[13px] font-display transition-colors ${
                  isActive(`/specs/${spec.entity_type}`)
                    ? "text-zinc-100 bg-white/[0.06]"
                    : "text-zinc-500 hover:text-zinc-300 hover:bg-white/[0.04]"
                }`}
              >
                <div className="flex items-center gap-2.5">
                  <NavIcon icon="file-text" />
                  <span className="truncate">{spec.entity_type}</span>
                </div>
                {entityCounts[spec.entity_type] !== undefined && (
                  <span className="text-[10px] font-mono bg-teal-500/10 text-teal-400 px-1.5 py-0.5 rounded-sm">
                    {entityCounts[spec.entity_type]}
                  </span>
                )}
              </Link>
            ))}
          </CollapsibleSection>
        ))}

        {/* Static verify / entities links */}
        <CollapsibleSection title="Tools" storageKey="tools">
          <Link
            href={specs.length > 0 ? `/verify/${specs[0].entity_type}` : "/verify/Ticket"}
            className={`flex items-center gap-2.5 px-2.5 py-1.5 rounded-sm text-[13px] font-display transition-colors ${
              pathname.startsWith("/verify")
                ? "text-zinc-100 bg-white/[0.06]"
                : "text-zinc-500 hover:text-zinc-300 hover:bg-white/[0.04]"
            }`}
          >
            <NavIcon icon="shield" />
            Verification
          </Link>
        </CollapsibleSection>
      </nav>

      {/* Connection Status + Footer */}
      <div className="px-4 py-3 mt-auto space-y-1.5">
        {!checking && (
          <div className="flex items-center gap-2">
            <div
              className={`w-1.5 h-1.5 rounded-full ${
                connected ? "bg-teal-400" : "bg-pink-400"
              }`}
            />
            <span className={`text-[11px] ${connected ? "text-zinc-600" : "text-pink-400"}`}>
              {connected ? "Connected" : "Disconnected"}
            </span>
          </div>
        )}
        <div className="text-[10px] text-zinc-700">v0.1.0</div>
      </div>
    </aside>
  );
}
