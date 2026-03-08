export default function LandingLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <div className="min-h-screen bg-[#0a0a0c] text-white overflow-x-hidden">
      {children}
    </div>
  );
}
