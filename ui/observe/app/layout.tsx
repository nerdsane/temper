import type { Metadata } from "next";
import { GeistMono } from "geist/font/mono";
import { GeistSans } from "geist/font/sans";
import "./globals.css";

export const metadata: Metadata = {
  title: "Temper",
  description: "The operating system for autonomous agents",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className={`dark ${GeistMono.variable} ${GeistSans.variable}`} suppressHydrationWarning>
      <body className={`${GeistSans.className} antialiased`} suppressHydrationWarning>
        {children}
      </body>
    </html>
  );
}
