"use client";

import { useState, useCallback, useEffect, useMemo } from "react";
import { fetchOsApps, installOsApp, fetchSpecs } from "@/lib/api";
import { usePolling } from "@/lib/hooks";
import type { OsAppsResponse, SpecSummary } from "@/lib/types";
import ErrorDisplay from "@/components/ErrorDisplay";
import StatCard from "@/components/StatCard";

export default function OsAppsPage() {
  const [initialLoading, setInitialLoading] = useState(true);
  const [initialError, setInitialError] = useState<string | null>(null);
  const [installing, setInstalling] = useState<string | null>(null);
  const [installResult, setInstallResult] = useState<{ app: string; status: string } | null>(null);

  const loadInitial = useCallback(async () => {
    setInitialLoading(true);
    setInitialError(null);
    try {
      await fetchOsApps();
    } catch (err) {
      setInitialError(err instanceof Error ? err.message : "Failed to load OS apps");
    } finally {
      setInitialLoading(false);
    }
  }, []);

  useEffect(() => {
    loadInitial();
  }, [loadInitial]);

  const appsPoll = usePolling<OsAppsResponse>({
    fetcher: fetchOsApps,
    interval: 10000,
    enabled: !initialLoading && !initialError,
  });

  const specsPoll = usePolling<SpecSummary[]>({
    fetcher: fetchSpecs,
    interval: 10000,
    enabled: !initialLoading && !initialError,
  });

  const apps = appsPoll.data;
  const specs = specsPoll.data;

  const loadedEntityTypes = useMemo(() => {
    if (!specs) return new Set<string>();
    return new Set(specs.map((s) => s.entity_type));
  }, [specs]);

  const installedCount = useMemo(() => {
    if (!apps?.apps) return 0;
    return apps.apps.filter((app) =>
      app.entity_types.every((et) => loadedEntityTypes.has(et)),
    ).length;
  }, [apps, loadedEntityTypes]);

  const handleInstall = async (appName: string) => {
    setInstalling(appName);
    setInstallResult(null);
    try {
      await installOsApp(appName, "default");
      setInstallResult({ app: appName, status: "installed" });
    } catch (err) {
      setInstallResult({
        app: appName,
        status: err instanceof Error ? err.message : "Install failed",
      });
    } finally {
      setInstalling(null);
    }
  };

  if (initialLoading) {
    return (
      <div className="animate-pulse">
        <div className="h-6 bg-[var(--color-border)] rounded w-40 mb-1.5" />
        <div className="h-3.5 bg-[var(--color-border)] rounded w-72 mb-6" />
        <div className="grid grid-cols-2 gap-3 mb-6">
          {[0, 1].map((i) => (
            <div key={i} className="glass rounded-[2px] p-4">
              <div className="h-3 bg-[var(--color-border)] rounded w-20 mb-2" />
              <div className="h-8 bg-[var(--color-border)] rounded w-10" />
            </div>
          ))}
        </div>
      </div>
    );
  }

  if (initialError) {
    return <ErrorDisplay title="Cannot load OS apps" message={initialError} retry={loadInitial} />;
  }

  return (
    <div className="animate-fade-in">
      {/* Header */}
      <div className="mb-6">
        <h1 className="text-2xl text-[var(--color-text-primary)] tracking-tight font-serif">OS Apps</h1>
        <p className="text-sm text-[var(--color-text-muted)] mt-0.5">
          Pre-built application specs ready to install
        </p>
      </div>

      {/* Stats */}
      <div className="grid grid-cols-2 gap-3 mb-6">
        <StatCard label="Available" value={apps?.apps.length ?? 0} />
        <StatCard label="Installed" value={installedCount} />
      </div>

      {/* Install result banner */}
      {installResult && (
        <div
          className={`mb-4 px-4 py-2.5 rounded text-[13px] ${
            installResult.status === "installed"
              ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] border border-[var(--color-accent-teal)]/20"
              : "bg-[var(--color-accent-pink-dim)] text-[var(--color-accent-pink)] border border-[var(--color-accent-pink)]/20"
          }`}
        >
          {installResult.status === "installed"
            ? `${installResult.app} installed successfully`
            : `Failed to install ${installResult.app}: ${installResult.status}`}
        </div>
      )}

      {/* App cards */}
      {apps && apps.apps.length > 0 ? (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
          {apps.apps.map((app) => {
            const isInstalled = app.entity_types.every((et) => loadedEntityTypes.has(et));
            const isInstalling = installing === app.name;

            return (
              <div
                key={app.name}
                className="glass rounded-[2px] p-5 flex flex-col gap-3"
              >
                {/* Title row */}
                <div className="flex items-start justify-between">
                  <div>
                    <h3 className="text-[15px] font-semibold text-[var(--color-text-primary)] tracking-tight">
                      {app.name}
                    </h3>
                    <span className="text-[10px] font-mono text-[var(--color-text-muted)]">v{app.version}</span>
                  </div>
                  {isInstalled ? (
                    <span className="text-[10px] font-medium bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)] px-2 py-1 rounded">
                      Installed
                    </span>
                  ) : (
                    <button
                      onClick={() => handleInstall(app.name)}
                      disabled={isInstalling}
                      className="text-[11px] font-medium bg-[var(--color-bg-elevated)] text-[var(--color-text-secondary)] px-3 py-1.5 rounded hover:bg-[var(--color-border)] transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                    >
                      {isInstalling ? "Installing..." : "Install"}
                    </button>
                  )}
                </div>

                {/* Description */}
                <p className="text-[13px] text-[var(--color-text-secondary)] leading-relaxed">
                  {app.description}
                </p>

                {/* Entity type chips */}
                <div className="flex flex-wrap gap-1.5">
                  {app.entity_types.map((et) => (
                    <span
                      key={et}
                      className={`text-[10px] font-mono px-2 py-0.5 rounded ${
                        loadedEntityTypes.has(et)
                          ? "bg-[var(--color-accent-teal-dim)] text-[var(--color-accent-teal)]"
                          : "bg-[var(--color-accent-lime-dim)] text-[var(--color-text-secondary)]"
                      }`}
                    >
                      {et}
                    </span>
                  ))}
                </div>
              </div>
            );
          })}
        </div>
      ) : (
        <div className="glass rounded-[2px] p-6 text-center">
          <p className="text-sm text-[var(--color-text-secondary)]">No OS apps available in the catalog.</p>
        </div>
      )}
    </div>
  );
}
