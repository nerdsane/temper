import type { NextConfig } from "next";

const TEMPER_API = process.env.TEMPER_API_URL || "http://localhost:3333";

const nextConfig: NextConfig = {
  async rewrites() {
    return [
      {
        source: "/observe/:path*",
        destination: `${TEMPER_API}/observe/:path*`,
      },
    ];
  },
};

export default nextConfig;
