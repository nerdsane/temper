import type { Metadata } from "next";
import { GeistSans } from "geist/font/sans";
import { GeistMono } from "geist/font/mono";
import { Space_Grotesk } from "next/font/google";
import "./globals.css";
import Sidebar from "@/components/Sidebar";
import ErrorBoundary from "@/components/ErrorBoundary";
import { ConnectionProvider } from "@/lib/connection";

const spaceGrotesk = Space_Grotesk({ subsets: ["latin"], variable: "--font-display" });

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
    <html lang="en" className={`dark ${GeistSans.variable} ${GeistMono.variable} ${spaceGrotesk.variable}`}>
      <body className={`${GeistSans.className} antialiased`}>
        <ConnectionProvider>
          <div className="flex h-screen overflow-hidden">
            <div className="flex-shrink-0">
              <Sidebar />
            </div>
            <main className="flex-1 overflow-y-auto">
              <div className="max-w-6xl mx-auto px-6 py-5">
                <ErrorBoundary>{children}</ErrorBoundary>
              </div>
            </main>
          </div>
        </ConnectionProvider>
      </body>
    </html>
  );
}
