import type { NextConfig } from "next";

const TEMPER_API = process.env.TEMPER_API_URL || "http://127.0.0.1:3333";

const nextConfig: NextConfig = {
  async rewrites() {
    return [
      {
        source: "/observe/:path*",
        destination: `${TEMPER_API}/observe/:path*`,
      },
      {
        source: "/api/:path*",
        destination: `${TEMPER_API}/api/:path*`,
      },
    ];
  },
};

export default nextConfig;
