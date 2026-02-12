import type { Metadata } from "next";
import "./globals.css";
import Sidebar from "@/components/Sidebar";
import ErrorBoundary from "@/components/ErrorBoundary";
import { ConnectionProvider } from "@/lib/connection";

export const metadata: Metadata = {
  title: "Temper Observe",
  description: "Observability dashboard for Temper — visualize specs, verification results, and entity lifecycles",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className="dark">
      <body className="antialiased">
        <ConnectionProvider>
          <div className="flex min-h-screen">
            <Sidebar />
            <main className="flex-1 overflow-auto">
              <div className="max-w-6xl mx-auto p-6">
                <ErrorBoundary>{children}</ErrorBoundary>
              </div>
            </main>
          </div>
        </ConnectionProvider>
      </body>
    </html>
  );
}
