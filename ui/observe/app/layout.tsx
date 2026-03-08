import type { Metadata } from "next";
import { GeistMono } from "geist/font/mono";
import { Space_Grotesk } from "next/font/google";
import "./globals.css";
import Sidebar from "@/components/Sidebar";
import ErrorBoundary from "@/components/ErrorBoundary";
import Providers from "@/components/Providers";
import { ConnectionProvider } from "@/lib/connection";
import { DecisionNotifierProvider } from "@/lib/decision-notifier";

const spaceGrotesk = Space_Grotesk({ subsets: ["latin"], variable: "--font-space-grotesk" });

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
    <html lang="en" className={`dark ${GeistMono.variable} ${spaceGrotesk.variable}`} suppressHydrationWarning>
      <body className={`${spaceGrotesk.className} antialiased`} suppressHydrationWarning>
        <Providers>
        <ConnectionProvider>
          <DecisionNotifierProvider>
            <div className="flex h-screen overflow-hidden">
              <div className="flex-shrink-0">
                <Sidebar />
              </div>
              <main className="flex-1 overflow-y-auto bg-[radial-gradient(ellipse_at_top_left,_#131320_0%,_#0d0d14_40%,_#0a0a0c_100%)]">
                <div className="max-w-6xl mx-auto px-6 py-5">
                  <ErrorBoundary>{children}</ErrorBoundary>
                </div>
              </main>
            </div>
          </DecisionNotifierProvider>
        </ConnectionProvider>
        </Providers>
      </body>
    </html>
  );
}
