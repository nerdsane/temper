import Sidebar from "@/components/Sidebar";
import ErrorBoundary from "@/components/ErrorBoundary";
import Providers from "@/components/Providers";
import { ConnectionProvider } from "@/lib/connection";
import { DecisionNotifierProvider } from "@/lib/decision-notifier";
import { SSERefreshProvider } from "@/lib/sse-context";
export default function ObserveLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <Providers>
      <SSERefreshProvider>
      <ConnectionProvider>
        <DecisionNotifierProvider>
          <div className="flex h-screen overflow-hidden">
            <div className="flex-shrink-0">
              <Sidebar />
            </div>
            <main className="flex-1 overflow-y-auto bg-[var(--color-bg-primary)]">
              <div className="grain-overlay" />
              <div className="max-w-6xl mx-auto px-6 py-5 relative z-10 animate-content-reveal">
                <ErrorBoundary>{children}</ErrorBoundary>
              </div>
            </main>
          </div>
        </DecisionNotifierProvider>
      </ConnectionProvider>
      </SSERefreshProvider>
    </Providers>
  );
}
