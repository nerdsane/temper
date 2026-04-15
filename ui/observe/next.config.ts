import type { NextConfig } from "next";

const TEMPER_API = process.env.TEMPER_API_URL || "http://127.0.0.1:3333";

const nextConfig: NextConfig = {
  eslint: {
    ignoreDuringBuilds: true,
  },
  images: {
    remotePatterns: [
      { protocol: "https", hostname: "avatars.githubusercontent.com" },
    ],
  },
  async rewrites() {
    return [
      {
        source: "/observe/:path*",
        destination: `${TEMPER_API}/observe/:path*`,
      },
      {
        // Proxy API requests to Temper server, but NOT auth routes
        // (handled by next-auth locally).
        source: "/api/:path((?!auth).*)",
        destination: `${TEMPER_API}/api/:path*`,
      },
    ];
  },
};

export default nextConfig;
