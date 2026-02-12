"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { fetchSpecs, fetchEntities } from "@/lib/api";
import { useConnection } from "@/lib/connection";
import type { SpecSummary } from "@/lib/types";

function NavIcon({ icon }: { icon: string }) {
  switch (icon) {
    case "grid":
      return (
        <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M4 5a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1H5a1 1 0 01-1-1V5zM14 5a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1h-4a1 1 0 01-1-1V5zM4 15a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1H5a1 1 0 01-1-1v-4zM14 15a1 1 0 011-1h4a1 1 0 011 1v4a1 1 0 01-1 1h-4a1 1 0 01-1-1v-4z" />
        </svg>
      );
    case "file-text":
      return (
        <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
        </svg>
      );
    case "shield":
      return (
        <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M9 12l2 2 4-4m5.618-4.016A11.955 11.955 0 0112 2.944a11.955 11.955 0 01-8.618 3.04A12.02 12.02 0 003 9c0 5.591 3.824 10.29 9 11.622 5.176-1.332 9-6.03 9-11.622 0-1.042-.133-2.052-.382-3.016z" />
        </svg>
      );
    case "box":
      return (
        <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
          <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5}
            d="M20 7l-8-4-8 4m16 0l-8 4m8-4v10l-8 4m0-10L4 7m8 4v10M4 7v10l8 4" />
        </svg>
      );
    default:
      return null;
  }
}

export default function Sidebar() {
  const pathname = usePathname();
  const { connected, checking } = useConnection();
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
  ];

  return (
    <aside className="w-56 bg-gray-950 border-r border-gray-800 flex flex-col min-h-screen">
      {/* Logo / Title */}
      <div className="p-4 border-b border-gray-800">
        <Link href="/" className="flex items-center gap-2">
          <div className="w-8 h-8 bg-blue-600 rounded flex items-center justify-center text-sm font-bold">
            T
          </div>
          <div>
            <div className="text-sm font-semibold text-gray-100">Temper</div>
            <div className="text-xs text-gray-500">Observe</div>
          </div>
        </Link>
      </div>

      {/* Navigation */}
      <nav className="flex-1 p-3 space-y-1 overflow-y-auto">
        {/* Static nav */}
        {staticItems.map((item) => (
          <Link
            key={item.href}
            href={item.href}
            className={`flex items-center gap-3 px-3 py-2 rounded-md text-sm transition-colors ${
              isActive(item.href)
                ? "bg-gray-800 text-white"
                : "text-gray-400 hover:text-gray-200 hover:bg-gray-900"
            }`}
          >
            <NavIcon icon={item.icon} />
            {item.label}
          </Link>
        ))}

        {/* Dynamic spec nav */}
        {specs.length > 0 && (
          <>
            <div className="pt-3 pb-1 px-3 text-xs font-medium text-gray-600 uppercase tracking-wider">
              Specs
            </div>
            {specs.map((spec) => (
              <Link
                key={spec.entity_type}
                href={`/specs/${spec.entity_type}`}
                className={`flex items-center justify-between px-3 py-2 rounded-md text-sm transition-colors ${
                  isActive(`/specs/${spec.entity_type}`)
                    ? "bg-gray-800 text-white"
                    : "text-gray-400 hover:text-gray-200 hover:bg-gray-900"
                }`}
              >
                <div className="flex items-center gap-3">
                  <NavIcon icon="file-text" />
                  <span className="truncate">{spec.entity_type}</span>
                </div>
                {entityCounts[spec.entity_type] !== undefined && (
                  <span className="text-xs font-mono bg-gray-800 text-gray-500 px-1.5 py-0.5 rounded">
                    {entityCounts[spec.entity_type]}
                  </span>
                )}
              </Link>
            ))}
          </>
        )}

        {/* Static verify / entities links */}
        <div className="pt-3 pb-1 px-3 text-xs font-medium text-gray-600 uppercase tracking-wider">
          Tools
        </div>
        <Link
          href={specs.length > 0 ? `/verify/${specs[0].entity_type}` : "/verify/Ticket"}
          className={`flex items-center gap-3 px-3 py-2 rounded-md text-sm transition-colors ${
            pathname.startsWith("/verify")
              ? "bg-gray-800 text-white"
              : "text-gray-400 hover:text-gray-200 hover:bg-gray-900"
          }`}
        >
          <NavIcon icon="shield" />
          Verification
        </Link>
      </nav>

      {/* Connection Status + Footer */}
      <div className="p-4 border-t border-gray-800 space-y-2">
        {!checking && (
          <div className="flex items-center gap-2">
            <div
              className={`w-2 h-2 rounded-full ${
                connected ? "bg-green-500" : "bg-red-500"
              }`}
            />
            <span className={`text-xs ${connected ? "text-gray-500" : "text-red-400"}`}>
              {connected ? "Connected" : "Disconnected"}
            </span>
          </div>
        )}
        <div className="text-xs text-gray-600">v0.1.0</div>
      </div>
    </aside>
  );
}
